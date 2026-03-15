// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

// mod added;
mod buttons;
mod card_counts;
mod eases;
mod future_due;
mod hours;
mod intervals;
mod memorized;
mod retention;
mod retrievability;
mod reviews;
mod today;

use std::collections::HashMap;

use anki_proto::stats::graphs_response::Added;
use fsrs::FSRS;

use crate::card::CardId;
use crate::config::BoolKey;
use crate::config::Weekday;
use crate::deckconfig::DeckConfigId;
use crate::prelude::*;
use crate::revlog::RevlogEntry;
use crate::search::SortMode;
use crate::stats::graphs::memorized::MemorizedContext;

struct GraphsContext {
    revlog: Vec<RevlogEntry>,
    cards: Vec<Card>,
    next_day_start: TimestampSecs,
    days_elapsed: u32,
    local_offset_secs: i64,
}

impl Collection {
    fn graph_context(&mut self, search: &str, days: u32) -> Result<GraphsContext> {
        let guard = self.search_cards_into_table(search, SortMode::NoOrder)?;
        let all = search.trim().is_empty();
        let timing = guard.col.timing_today()?;
        let revlog_start = if days > 0 {
            timing
                .next_day_at
                .adding_secs(-(((days as i64) + 1) * 86_400))
        } else {
            TimestampSecs(0)
        };
        let offset = guard.col.local_utc_offset_for_user()?;
        let local_offset_secs = offset.local_minus_utc() as i64;
        let revlog = if all {
            guard.col.storage.get_all_revlog_entries(revlog_start)?
        } else {
            guard
                .col
                .storage
                .get_revlog_entries_for_searched_cards_after_stamp(revlog_start)?
        };
        Ok(GraphsContext {
            revlog,
            days_elapsed: timing.days_elapsed,
            cards: guard.col.storage.all_searched_cards()?,
            next_day_start: timing.next_day_at,
            local_offset_secs,
        })
    }

    pub(crate) fn graph_data_for_search(
        &mut self,
        search: &str,
        days: u32,
    ) -> Result<anki_proto::stats::GraphsResponse> {
        let ctx = self.graph_context(search, days)?;
        self.graph_data(ctx)
    }

    fn graph_data(&mut self, ctx: GraphsContext) -> Result<anki_proto::stats::GraphsResponse> {
        let (eases, difficulty) = ctx.eases();

        let ctx = self.build_memorized_context(ctx)?;
        let memorized = {
            Added {
                added: ctx.historical_fsrs()?,
            }
        };
        let ctx = ctx.graph_context;
        let resp = anki_proto::stats::GraphsResponse {
            added: Some(memorized),
            reviews: Some(ctx.review_counts_and_times()),
            true_retention: Some(ctx.calculate_true_retention()),
            future_due: Some(ctx.future_due()),
            intervals: Some(ctx.intervals()),
            stability: Some(ctx.stability()),
            eases: Some(eases),
            difficulty: Some(difficulty),
            today: Some(ctx.today()),
            hours: Some(ctx.hours()),
            buttons: Some(ctx.buttons()),
            card_counts: Some(ctx.card_counts()),
            rollover_hour: self.rollover_for_current_scheduler()? as u32,
            retrievability: Some(ctx.retrievability()),
            fsrs: self.get_config_bool(BoolKey::Fsrs),
        };
        Ok(resp)
    }

    pub(crate) fn get_graph_preferences(&self) -> anki_proto::stats::GraphPreferences {
        anki_proto::stats::GraphPreferences {
            calendar_first_day_of_week: self.get_first_day_of_week() as i32,
            card_counts_separate_inactive: self
                .get_config_bool(BoolKey::CardCountsSeparateInactive),
            browser_links_supported: true,
            future_due_show_backlog: self.get_config_bool(BoolKey::FutureDueShowBacklog),
        }
    }

    pub(crate) fn set_graph_preferences(
        &mut self,
        prefs: anki_proto::stats::GraphPreferences,
    ) -> Result<()> {
        self.set_first_day_of_week(match prefs.calendar_first_day_of_week {
            1 => Weekday::Monday,
            5 => Weekday::Friday,
            6 => Weekday::Saturday,
            _ => Weekday::Sunday,
        })?;
        self.set_config_bool_inner(
            BoolKey::CardCountsSeparateInactive,
            prefs.card_counts_separate_inactive,
        )?;
        self.set_config_bool_inner(BoolKey::FutureDueShowBacklog, prefs.future_due_show_backlog)?;
        Ok(())
    }

    fn memorized_context(&mut self, search: &str) -> Result<MemorizedContext> {
        let ctx = self.graph_context(search, 0)?;
        self.build_memorized_context(ctx)
    }
    fn build_memorized_context(&mut self, ctx: GraphsContext) -> Result<MemorizedContext> {
        // Build one FSRS instance per deck config preset using the actual stored
        // parameters (FSRS-6 → FSRS-5 → FSRS-4 → empty = crate defaults),
        // matching the getFsrs(config) logic in Anki-Search-Stats-Extended.
        let config_map = self.storage.get_deck_config_map()?;
        let mut per_preset_fsrs: HashMap<DeckConfigId, FSRS> =
            HashMap::with_capacity(config_map.len());
        for (dcid, config) in &config_map {
            per_preset_fsrs.insert(*dcid, FSRS::new(Some(config.fsrs_params()))?);
        }
        if per_preset_fsrs.is_empty() {
            per_preset_fsrs.insert(DeckConfigId(0), FSRS::new(Some(&[]))?);
        }

        // Map card ID → DeckConfigId, respecting original_deck_id for filtered decks.
        let decks_map = self.storage.get_decks_map()?;
        let mut card_config_map: HashMap<CardId, DeckConfigId> =
            HashMap::with_capacity(ctx.cards.len());
        for card in &ctx.cards {
            let deck_id = if card.original_deck_id.0 != 0 {
                card.original_deck_id
            } else {
                card.deck_id
            };
            if let Some(deck) = decks_map.get(&deck_id) {
                if let Some(conf_id) = deck.config_id() {
                    card_config_map.insert(card.id, conf_id);
                }
            }
        }

        Ok(MemorizedContext {
            graph_context: ctx,
            per_preset_fsrs,
            card_config_map,
        })
    }
}
