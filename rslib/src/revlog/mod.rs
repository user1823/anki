// Copyright: Ankitects Pty Ltd and contributors
// License: GNU AGPL, version 3 or later; http://www.gnu.org/licenses/agpl.html

pub(crate) mod undo;

use std::cmp;

use num_enum::TryFromPrimitive;
use serde::Deserialize;
use serde_repr::Deserialize_repr;
use serde_repr::Serialize_repr;
use serde_tuple::Serialize_tuple;

use crate::define_newtype;
use crate::prelude::*;
use crate::serde::default_on_invalid;
use crate::serde::deserialize_int_from_number;

define_newtype!(RevlogId, i64);

const REVLOG_CHUNK_CARD_COUNT: usize = 4096;
const REVLOG_CHUNK_READ_RETRIES: usize = 2;

impl RevlogId {
    pub fn new() -> Self {
        RevlogId(TimestampMillis::now().0)
    }

    pub fn as_secs(self) -> TimestampSecs {
        TimestampSecs(self.0 / 1000)
    }
}

impl From<TimestampMillis> for RevlogId {
    fn from(m: TimestampMillis) -> Self {
        RevlogId(m.0)
    }
}

#[derive(Serialize_tuple, Deserialize, Debug, Default, PartialEq, Eq, Clone)]
pub struct RevlogEntry {
    pub id: RevlogId,
    pub cid: CardId,
    pub usn: Usn,
    /// - In the V1 scheduler, 3 represents easy in the learning case.
    /// - 0 represents manual rescheduling.
    #[serde(rename = "ease")]
    pub button_chosen: u8,
    /// Positive values are in days, negative values in seconds.
    #[serde(rename = "ivl", deserialize_with = "deserialize_int_from_number")]
    pub interval: i32,
    /// Positive values are in days, negative values in seconds.
    #[serde(rename = "lastIvl", deserialize_with = "deserialize_int_from_number")]
    pub last_interval: i32,
    /// Card's ease after answering, stored as 10x the %, eg 2500 represents
    /// 250%. When FSRS is active, difficulty is normalized to 100-1100 range,
    /// so a 0 difficulty can be distinguished from SM-2 learning.
    #[serde(rename = "factor", deserialize_with = "deserialize_int_from_number")]
    pub ease_factor: u32,
    /// Amount of milliseconds taken to answer the card.
    #[serde(rename = "time", deserialize_with = "deserialize_int_from_number")]
    pub taken_millis: u32,
    #[serde(rename = "type", default, deserialize_with = "default_on_invalid")]
    pub review_kind: RevlogReviewKind,
}

#[derive(Serialize_repr, Deserialize_repr, Debug, PartialEq, Eq, TryFromPrimitive, Clone, Copy)]
#[repr(u8)]
#[derive(Default)]
pub enum RevlogReviewKind {
    #[default]
    Learning = 0,
    Review = 1,
    Relearning = 2,
    /// Old Anki versions called this "Cram" or "Early". It's assigned when
    /// reviewing cards before they're due, or when rescheduling is
    /// disabled.
    Filtered = 3,
    Manual = 4,
    Rescheduled = 5,
}

impl RevlogEntry {
    pub(crate) fn interval_secs(&self) -> u32 {
        u32::try_from(if self.interval > 0 {
            self.interval.saturating_mul(86_400)
        } else {
            self.interval.saturating_mul(-1)
        })
        .unwrap()
    }

    pub(crate) fn last_interval_secs(&self) -> u32 {
        u32::try_from(if self.last_interval > 0 {
            self.last_interval.saturating_mul(86_400)
        } else {
            self.last_interval.saturating_mul(-1)
        })
        .unwrap()
    }

    /// Returns true if this entry represents a reset operation.
    /// These entries are created when a card is reset using
    /// [`Collection::reschedule_cards_as_new`].
    /// The 0 value of `ease_factor` differentiates it
    /// from entry created by [`Collection::set_due_date`] that has
    /// `RevlogReviewKind::Manual` but non-zero `ease_factor`.
    pub(crate) fn is_reset(&self) -> bool {
        self.review_kind == RevlogReviewKind::Manual && self.ease_factor == 0
    }

    /// Returns true if this entry represents a cramming operation.
    /// These entries are created when a card is reviewed in a
    /// filtered deck with "Reschedule cards based on my answers
    /// in this deck" disabled.
    /// [`crate::scheduler::answering::CardStateUpdater::apply_preview_state`].
    /// The 0 value of `ease_factor` distinguishes it from the entry
    /// created when a card is reviewed before its due date in a
    /// filtered deck with reschedule enabled or using Grade Now.
    pub(crate) fn is_cramming(&self) -> bool {
        self.review_kind == RevlogReviewKind::Filtered && self.ease_factor == 0
    }

    pub(crate) fn has_rating(&self) -> bool {
        self.button_chosen > 0
    }

    /// Returns true if the review entry is not manually rescheduled and not
    /// cramming. Used to filter out entries that shouldn't be considered
    /// for statistics and scheduling.
    pub(crate) fn has_rating_and_affects_scheduling(&self) -> bool {
        // not rescheduled/set due date/reset
        self.has_rating()
            // not cramming
            && !self.is_cramming()
    }
}

impl Collection {
    pub(crate) fn all_revlog_entries_in_card_order_chunked(
        &mut self,
        after: TimestampSecs,
    ) -> Result<Vec<RevlogEntry>> {
        self.revlog_entries_in_card_order_chunked(
            |col, after_cid, chunk_size| {
                col.storage
                    .get_all_revlog_card_ids_after_stamp_chunk(after, after_cid, chunk_size)
            },
            |col, cids| {
                col.storage
                    .get_revlog_entries_for_card_ids_in_card_order_after_stamp(cids, after)
            },
            |col| col.storage.get_all_revlog_entries_in_card_order_after_stamp(after),
            |_| Ok(()),
        )
    }

    pub(crate) fn searched_revlog_entries_in_card_order_chunked(
        &mut self,
        after: TimestampSecs,
    ) -> Result<Vec<RevlogEntry>> {
        self.revlog_entries_in_card_order_chunked(
            |col, after_cid, chunk_size| {
                col.storage
                    .get_searched_revlog_card_ids_after_stamp_chunk(after, after_cid, chunk_size)
            },
            |col, cids| {
                col.storage
                    .get_revlog_entries_for_card_ids_in_card_order_after_stamp(cids, after)
            },
            |col| {
                col.storage
                    .get_revlog_entries_for_searched_cards_in_card_order_after_stamp(after)
            },
            |_| Ok(()),
        )
    }

    pub(crate) fn revlog_entries_in_card_order_chunked<
        FCardIds,
        FEntries,
        FFallback,
        FAfterChunk,
    >(
        &mut self,
        mut next_card_ids: FCardIds,
        mut entries_for_cards: FEntries,
        mut fallback: FFallback,
        mut after_chunk: FAfterChunk,
    ) -> Result<Vec<RevlogEntry>>
    where
        FCardIds: FnMut(&mut Collection, CardId, usize) -> Result<Vec<CardId>>,
        FEntries: FnMut(&mut Collection, &[CardId]) -> Result<Vec<RevlogEntry>>,
        FFallback: FnMut(&mut Collection) -> Result<Vec<RevlogEntry>>,
        FAfterChunk: FnMut(&mut Collection) -> Result<()>,
    {
        for _attempt in 0..=REVLOG_CHUNK_READ_RETRIES {
            let change_stamp = self.changes_since_open()?;
            let mut out = Vec::new();
            let mut after_cid = CardId(0);
            let mut changed_mid_read = false;

            loop {
                let card_ids = next_card_ids(self, after_cid, REVLOG_CHUNK_CARD_COUNT)?;
                if card_ids.is_empty() {
                    return Ok(out);
                }

                out.extend(entries_for_cards(self, &card_ids)?);
                after_cid = *card_ids.last().unwrap();
                after_chunk(self)?;

                if self.changes_since_open()? != change_stamp {
                    changed_mid_read = true;
                    break;
                }
            }

            if !changed_mid_read {
                return Ok(out);
            }
        }

        fallback(self)
    }

    // set due date or reset
    pub(crate) fn log_manually_scheduled_review(
        &mut self,
        card: &Card,
        original_interval: u32,
        usn: Usn,
    ) -> Result<()> {
        self.log_scheduled_review(card, original_interval, usn, RevlogReviewKind::Manual)
    }

    // reschedule cards on change
    pub(crate) fn log_rescheduled_review(
        &mut self,
        card: &Card,
        original_interval: u32,
        usn: Usn,
    ) -> Result<()> {
        self.log_scheduled_review(card, original_interval, usn, RevlogReviewKind::Rescheduled)
    }

    fn log_scheduled_review(
        &mut self,
        card: &Card,
        original_interval: u32,
        usn: Usn,
        review_kind: RevlogReviewKind,
    ) -> Result<()> {
        let ease_factor = u32::from(
            card.memory_state
                .map(|s| (s.difficulty_shifted() * 1000.) as u16)
                .unwrap_or(card.ease_factor),
        );
        let entry = RevlogEntry {
            id: RevlogId::new(),
            cid: card.id,
            usn,
            button_chosen: 0,
            interval: i32::try_from(card.interval).unwrap_or(i32::MAX),
            last_interval: i32::try_from(original_interval).unwrap_or(i32::MAX),
            ease_factor,
            taken_millis: 0,
            review_kind,
        };
        self.add_revlog_entry_undoable(entry)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::time::Instant;

    use super::*;
    use crate::card::CardQueue;
    use crate::card::CardType;
    use crate::tests::NoteAdder;

    fn add_cards_with_reviews(
        col: &mut Collection,
        cards: usize,
        reviews_per_card: usize,
    ) -> Result<Vec<CardId>> {
        let mut cids = Vec::with_capacity(cards);
        let mut revlog_id = 1_000_i64;

        for idx in 0..cards {
            let note = NoteAdder::basic(col)
                .fields(&[&format!("front-{idx}"), "back"])
                .add(col);
            let mut card = col.storage.all_cards_of_note(note.id)?.into_iter().next().unwrap();
            card.ctype = CardType::Review;
            card.queue = CardQueue::Review;
            card.interval = 10;
            col.storage.update_card(&card)?;
            cids.push(card.id);

            for review_idx in 0..reviews_per_card {
                col.storage.add_revlog_entry(
                    &RevlogEntry {
                        id: RevlogId(revlog_id),
                        cid: card.id,
                        button_chosen: 3,
                        interval: 10 + review_idx as i32,
                        last_interval: 9 + review_idx as i32,
                        ease_factor: 2500,
                        taken_millis: 1000,
                        review_kind: RevlogReviewKind::Review,
                        ..Default::default()
                    },
                    false,
                )?;
                revlog_id += 1;
            }
        }

        Ok(cids)
    }

    #[test]
    fn searched_chunked_revlog_reader_matches_single_pass() -> Result<()> {
        let mut col = Collection::new();
        let cids = add_cards_with_reviews(&mut col, 32, 4)?;

        let expected = col.storage.with_searched_cards_table(false, || {
            col.storage.set_search_table_to_card_ids(&cids)?;
            col.storage
                .get_revlog_entries_for_searched_cards_in_card_order_after_stamp(0.into())
        })?;
        let actual = col.storage.with_searched_cards_table(false, || {
            col.storage.set_search_table_to_card_ids(&cids)?;
            col.searched_revlog_entries_in_card_order_chunked(0.into())
        })?;

        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn chunked_revlog_reader_restarts_after_write() -> Result<()> {
        let mut col = Collection::new();
        add_cards_with_reviews(&mut col, cmp::max(REVLOG_CHUNK_CARD_COUNT + 1, 4097), 1)?;
        let expected = col.all_revlog_entries_in_card_order_chunked(0.into())?;
        let mut inserted = false;

        let actual = col.revlog_entries_in_card_order_chunked(
            |col, after_cid, chunk_size| {
                col.storage
                    .get_all_revlog_card_ids_after_stamp_chunk(0.into(), after_cid, chunk_size)
            },
            |col, cids| {
                col.storage
                    .get_revlog_entries_for_card_ids_in_card_order_after_stamp(cids, 0.into())
            },
            |col| col.storage.get_all_revlog_entries_in_card_order_after_stamp(0.into()),
            |col| {
                if !inserted {
                    inserted = true;
                    col.transact_no_undo(|col| {
                        col.storage.add_revlog_entry(
                            &RevlogEntry {
                                id: RevlogId::new(),
                                cid: CardId(i64::MAX - 1),
                                button_chosen: 3,
                                interval: 10,
                                last_interval: 9,
                                ease_factor: 2500,
                                taken_millis: 1000,
                                review_kind: RevlogReviewKind::Review,
                                ..Default::default()
                            },
                            false,
                        )?;
                        Ok(())
                    })?;
                }
                Ok(())
            },
        )?;

        assert_eq!(actual.len(), expected.len() + 1);
        assert_eq!(actual.last().unwrap().cid, CardId(i64::MAX - 1));
        Ok(())
    }

    #[test]
    #[ignore]
    fn bench_chunked_revlog_reader_reports_max_chunk_time() -> Result<()> {
        let mut col = Collection::new();
        let cids = add_cards_with_reviews(&mut col, 20_000, 3)?;
        let full_start = Instant::now();
        let full = col.storage.with_searched_cards_table(false, || {
            col.storage.set_search_table_to_card_ids(&cids)?;
            col.storage
                .get_revlog_entries_for_searched_cards_in_card_order_after_stamp(0.into())
        })?;
        let full_elapsed = full_start.elapsed();

        let mut max_chunk = Duration::ZERO;
        let chunked_start = Instant::now();
        let chunked = col.storage.with_searched_cards_table(false, || {
            col.storage.set_search_table_to_card_ids(&cids)?;
            col.revlog_entries_in_card_order_chunked(
                |col, after_cid, chunk_size| {
                    col.storage
                        .get_searched_revlog_card_ids_after_stamp_chunk(0.into(), after_cid, chunk_size)
                },
                |col, cids| {
                    let started = Instant::now();
                    let out = col
                        .storage
                        .get_revlog_entries_for_card_ids_in_card_order_after_stamp(cids, 0.into())?;
                    max_chunk = max_chunk.max(started.elapsed());
                    Ok(out)
                },
                |col| {
                    col.storage
                        .get_revlog_entries_for_searched_cards_in_card_order_after_stamp(0.into())
                },
                |_| Ok(()),
            )
        })?;
        let chunked_elapsed = chunked_start.elapsed();

        assert_eq!(full, chunked);
        println!(
            "full={:?} chunked_total={:?} chunked_max={:?}",
            full_elapsed, chunked_elapsed, max_chunk
        );
        Ok(())
    }
}
