mod build_lex_table;
mod build_parse_table;
mod coincident_tokens;
mod item;
mod item_set_builder;
mod shrink_parse_table;
mod token_conflicts;

use self::build_lex_table::build_lex_table;
use self::build_parse_table::build_parse_table;
use self::coincident_tokens::CoincidentTokenIndex;
use self::item::LookaheadSet;
use self::shrink_parse_table::shrink_parse_table;
use self::token_conflicts::TokenConflictMap;
use crate::error::Result;
use crate::grammars::{InlinedProductionMap, LexicalGrammar, SyntaxGrammar};
use crate::nfa::{CharacterSet, NfaCursor};
use crate::rules::{AliasMap, Symbol};
use crate::tables::{LexTable, ParseAction, ParseTable, ParseTableEntry};

pub(crate) fn build_tables(
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    simple_aliases: &AliasMap,
    inlines: &InlinedProductionMap,
) -> Result<(ParseTable, LexTable, LexTable, Option<Symbol>)> {
    let (mut parse_table, following_tokens) =
        build_parse_table(syntax_grammar, lexical_grammar, inlines)?;
    let token_conflict_map = TokenConflictMap::new(lexical_grammar, following_tokens);
    let coincident_token_index = CoincidentTokenIndex::new(&parse_table, lexical_grammar);
    let keywords = identify_keywords(
        lexical_grammar,
        &parse_table,
        syntax_grammar.word_token,
        &token_conflict_map,
        &coincident_token_index,
    );
    populate_error_state(
        &mut parse_table,
        syntax_grammar,
        lexical_grammar,
        &coincident_token_index,
        &token_conflict_map,
    );
    shrink_parse_table(
        &mut parse_table,
        syntax_grammar,
        simple_aliases,
        &token_conflict_map,
    );
    let (main_lex_table, keyword_lex_table) =
        build_lex_table(&mut parse_table, syntax_grammar, lexical_grammar, &keywords);
    Ok((
        parse_table,
        main_lex_table,
        keyword_lex_table,
        syntax_grammar.word_token,
    ))
}

fn populate_error_state(
    parse_table: &mut ParseTable,
    syntax_grammar: &SyntaxGrammar,
    lexical_grammar: &LexicalGrammar,
    coincident_token_index: &CoincidentTokenIndex,
    token_conflict_map: &TokenConflictMap,
) {
    let state = &mut parse_table.states[0];
    let n = lexical_grammar.variables.len();
    let conflict_free_tokens = LookaheadSet::with((0..n).into_iter().filter_map(|i| {
        let conflicts_with_other_tokens = (0..n).into_iter().all(|j| {
            j == i
                || coincident_token_index.contains(Symbol::terminal(i), Symbol::terminal(j))
                || !token_conflict_map.does_conflict(i, j)
        });
        if conflicts_with_other_tokens {
            None
        } else {
            Some(Symbol::terminal(i))
        }
    }));

    let recover_entry = ParseTableEntry {
        reusable: false,
        actions: vec![ParseAction::Recover],
    };

    for i in 0..n {
        let symbol = Symbol::terminal(i);
        let can_be_used_for_recovery = conflict_free_tokens.contains(&symbol)
            || conflict_free_tokens.iter().all(|t| {
                coincident_token_index.contains(symbol, t)
                    || !token_conflict_map.does_conflict(i, t.index)
            });
        if can_be_used_for_recovery {
            state
                .terminal_entries
                .entry(symbol)
                .or_insert_with(|| recover_entry.clone());
        }
    }

    for (i, external_token) in syntax_grammar.external_tokens.iter().enumerate() {
        if external_token.corresponding_internal_token.is_none() {
            state
                .terminal_entries
                .entry(Symbol::external(i))
                .or_insert_with(|| recover_entry.clone());
        }
    }

    state.terminal_entries.insert(Symbol::end(), recover_entry);
}

fn identify_keywords(
    lexical_grammar: &LexicalGrammar,
    parse_table: &ParseTable,
    word_token: Option<Symbol>,
    token_conflict_map: &TokenConflictMap,
    coincident_token_index: &CoincidentTokenIndex,
) -> LookaheadSet {
    if word_token.is_none() {
        return LookaheadSet::new();
    }

    let word_token = word_token.unwrap();
    let mut cursor = NfaCursor::new(&lexical_grammar.nfa, Vec::new());

    // First find all of the candidate keyword tokens: tokens that start with
    // letters or underscore and can match the same string as a word token.
    let keywords = LookaheadSet::with(lexical_grammar.variables.iter().enumerate().filter_map(
        |(i, variable)| {
            cursor.reset(vec![variable.start_state]);
            if all_chars_are_alphabetical(&cursor)
                && token_conflict_map.does_match_same_string(i, word_token.index)
            {
                info!("Keywords - add candidate {}", lexical_grammar.variables[i].name);
                Some(Symbol::terminal(i))
            } else {
                None
            }
        },
    ));

    // Exclude keyword candidates that shadow another keyword candidate.
    let keywords = LookaheadSet::with(keywords.iter().filter(|token| {
        for other_token in keywords.iter() {
            if other_token != *token
                && token_conflict_map.does_match_same_string(token.index, other_token.index)
            {
                info!(
                    "Keywords - exclude {} because it matches the same string as {}",
                    lexical_grammar.variables[token.index].name,
                    lexical_grammar.variables[other_token.index].name
                );
                return false;
            }
        }
        true
    }));

    // Exclude keyword candidates for which substituting the keyword capture
    // token would introduce new lexical conflicts with other tokens.
    let keywords = LookaheadSet::with(keywords.iter().filter(|token| {
        for other_index in 0..lexical_grammar.variables.len() {
            if keywords.contains(&Symbol::terminal(other_index)) {
                continue;
            }

            // If the word token was already valid in every state containing
            // this keyword candidate, then substituting the word token won't
            // introduce any new lexical conflicts.
            if coincident_token_index
                .states_with(*token, Symbol::terminal(other_index))
                .iter()
                .all(|state_id| {
                    parse_table.states[*state_id]
                        .terminal_entries
                        .contains_key(&word_token)
                })
            {
                continue;
            }

            if !token_conflict_map.has_same_conflict_status(
                token.index,
                word_token.index,
                other_index,
            ) {
                info!(
                    "Keywords - exclude {} because of conflict with {}",
                    lexical_grammar.variables[token.index].name,
                    lexical_grammar.variables[other_index].name
                );
                return false;
            }
        }

        info!(
            "Keywords - include {}",
            lexical_grammar.variables[token.index].name,
        );
        true
    }));

    keywords
}

fn all_chars_are_alphabetical(cursor: &NfaCursor) -> bool {
    cursor.successors().all(|(chars, _, _, is_sep)| {
        if is_sep {
            true
        } else if let CharacterSet::Include(chars) = chars {
            chars.iter().all(|c| c.is_alphabetic() || *c == '_')
        } else {
            false
        }
    })
}
