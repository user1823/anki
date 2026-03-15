// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html
use std::collections::HashMap;

use fsrs::FSRS;
use itertools::izip;
use itertools::Itertools;

use super::GraphsContext;
use crate::card::CardId;
use crate::deckconfig::DeckConfigId;
use crate::prelude::*;
use crate::scheduler::fsrs::memory_state::fsrs_item_for_memory_state;

pub(crate) struct MemorizedContext {
    pub graph_context: GraphsContext,
    pub per_preset_fsrs: HashMap<DeckConfigId, FSRS>,
    pub card_config_map: HashMap<CardId, DeckConfigId>,
}

impl MemorizedContext {
    fn fsrs_for_card(&self, card_id: CardId) -> &FSRS {
        self.card_config_map
            .get(&card_id)
            .and_then(|dcid| self.per_preset_fsrs.get(dcid))
            .or_else(|| self.per_preset_fsrs.values().next())
            .expect("per_preset_fsrs must have at least one entry")
    }

    pub fn historical_fsrs(&self) -> Result<HashMap<i32, f32>> {
        let gctx = &self.graph_context;

        let card_logs = gctx.revlog.clone().into_iter().chunk_by(|r| r.cid);

        // Collect per-card items, pairing each card with its own FSRS instance.
        let items: Vec<(CardId, _)> = card_logs
            .into_iter()
            .filter_map(|(card_id, group)| {
                let item = fsrs_item_for_memory_state(
                    self.fsrs_for_card(card_id),
                    group.collect_vec(),
                    gctx.next_day_start,
                    0.9,
                    0.into(),
                )
                .ok()?;
                Some((card_id, item))
            })
            .collect();

        // Group cards by their deck config so we can call
        // historical_memory_state_batch once per unique parameter set.
        let mut grouped: HashMap<DeckConfigId, (Vec<_>, Vec<_>, Vec<_>)> = HashMap::new();
        for (card_id, maybe_item) in items {
            if let Some(item) = maybe_item {
                let config_id = self
                    .card_config_map
                    .get(&card_id)
                    .copied()
                    .unwrap_or(DeckConfigId(0));
                let (starting_states, filtered_revlogs, fsrs_items) =
                    grouped.entry(config_id).or_default();
                starting_states.push(item.starting_state);
                filtered_revlogs.push(item.filtered_revlogs);
                fsrs_items.push(item.item);
            }
        }
        let mut retention = HashMap::new();
        for (config_id, (starting_states, filtered_revlogs, fsrs_items)) in grouped {
            let fsrs = self
                .per_preset_fsrs
                .get(&config_id)
                .or_else(|| self.per_preset_fsrs.values().next())
                .expect("per_preset_fsrs must have at least one entry");
            let memory_states =
                fsrs.historical_memory_state_batch(fsrs_items, Some(starting_states))?;
            for (revlogs, memory_states) in izip![filtered_revlogs, memory_states] {
                for (from_to, memory_state) in izip!(
                    revlogs
                        .into_iter()
                        .map(|r| r.days_elapsed(gctx.next_day_start) as usize)
                        .chain([0])
                        .collect_vec()
                        .windows(2),
                    memory_states
                ) {
                    let start_day = from_to[1];
                    let end_day = from_to[0];
                    for i in start_day..end_day {
                        *retention.entry(-(i as i32)).or_default() +=
                            fsrs.current_retrievability(memory_state, (i - start_day) as u32, 0.2);
                    }
                }
            }
        }

        Ok(retention)
    }
}
