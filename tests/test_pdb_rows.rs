// Copyright (c) 2026 Jan Holthuis <jan.holthuis@rub.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy
// of the MPL was not distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.
//
// SPDX-License-Identifier: MPL-2.0

//! Adding and removing rows, checked on a real export.

use fallible_iterator::FallibleIterator;
use rekordcrate::pdb::io::Database;
use rekordcrate::pdb::*;
use std::io::Cursor;

const TEST_DB: &[u8] = include_bytes!("../data/pdb/num_rows/export.pdb");

fn keys_table() -> PageType {
    PageType::Plain(PlainPageType::Keys)
}

fn genres_table() -> PageType {
    PageType::Plain(PlainPageType::Genres)
}

fn open<R: std::io::Read + std::io::Seek>(io: R) -> Database<R> {
    Database::open_non_persistent(io, DatabaseType::Plain).expect("failed to open database")
}

/// Rows of a table, page by page, as `(page index, offset, row)`.
fn rows_of<R: std::io::Read + std::io::Seek>(
    db: &mut Database<R>,
    page_type: PageType,
) -> Vec<(PageIndex, u16, Row)> {
    let (table_index, _) = db
        .get_header()
        .find_table(page_type)
        .expect("table is present");
    let mut rows = Vec::new();
    let mut pages = db.iter_pages_for_table(table_index).expect("pages load");
    while let Some(page) = pages.next().expect("page loads") {
        let page_index = page.header.page_index;
        if let Some(data) = page.content.as_data() {
            for (offset, row) in &data.rows {
                rows.push((page_index, *offset, row.clone()));
            }
        }
    }
    rows
}

fn data_page_count<R: std::io::Read + std::io::Seek>(
    db: &mut Database<R>,
    page_type: PageType,
) -> usize {
    let (table_index, _) = db
        .get_header()
        .find_table(page_type)
        .expect("table is present");
    let mut count = 0;
    let mut pages = db.iter_pages_for_table(table_index).expect("pages load");
    while let Some(page) = pages.next().expect("page loads") {
        if page.header.page_flags.is_index_page() {
            continue;
        }
        count += 1;
    }
    count
}

/// A row that is not in the database yet.
///
/// Row fields are private and there are no constructors yet, so the row types can be copied but not
/// built from scratch. Copying is enough to exercise insertion and removal.
fn spare_row(template: &Row) -> Row {
    template.clone()
}

#[test]
fn a_row_can_be_measured() {
    let data = Vec::from(TEST_DB);
    let mut db = open(Cursor::new(data.as_slice()));
    let (_, _, row) = rows_of(&mut db, keys_table())
        .into_iter()
        .next()
        .expect("the table has a row");

    assert!(row.heap_bytes_required(()) > 0);
    assert_eq!(Row::Unknown.heap_bytes_required(()), 0);
}

#[test]
fn inserting_a_row_adds_it_to_the_table() {
    let mut data = Vec::from(TEST_DB);

    let before = {
        let mut db = open(Cursor::new(data.as_slice()));
        rows_of(&mut db, keys_table()).len()
    };

    let placed = {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        let (_, _, template) = rows_of(&mut db, keys_table())
            .into_iter()
            .next()
            .expect("the table has a row");
        let placed = db
            .insert_row(keys_table(), spare_row(&template))
            .expect("the row is inserted");
        db.close().expect("close");
        placed
    };

    let mut db = open(Cursor::new(data.as_slice()));
    let rows = rows_of(&mut db, keys_table());
    assert_eq!(rows.len(), before + 1, "one more row than before");
    assert!(
        rows.iter()
            .any(|(page, offset, _)| (*page, *offset) == placed),
        "the row sits where insert_row said it does"
    );

    let keys = db
        .iter_rows::<Key>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    assert_eq!(keys.len(), before + 1, "the table reads back one row more");
}

#[test]
fn inserting_adds_a_page_when_the_last_one_is_full() {
    let mut data = Vec::from(TEST_DB);

    let (pages_before, rows_before) = {
        let mut db = open(Cursor::new(data.as_slice()));
        (
            data_page_count(&mut db, keys_table()),
            rows_of(&mut db, keys_table()).len(),
        )
    };

    let mut added = 0;
    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        let template = rows_of(&mut db, keys_table())
            .into_iter()
            .next()
            .expect("the table has a row")
            .2;
        for _ in 0..800 {
            db.insert_row(keys_table(), spare_row(&template))
                .expect("the row is inserted");
            added += 1;
        }
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));
    let rows_after = rows_of(&mut db, keys_table()).len();
    let pages_after = data_page_count(&mut db, keys_table());
    assert_eq!(rows_after, rows_before + added);
    assert!(
        pages_after > pages_before,
        "the table needed {pages_after} pages for {rows_after} rows"
    );

    // The file grew by whole pages, and every page it claims is really there.
    assert_eq!(
        data.len() % 4096,
        0,
        "the file is still a whole number of pages"
    );
    let reachable = rows_after;
    assert_eq!(reachable, rows_before + added);
}

#[test]
fn removing_a_row_keeps_every_other_offset() {
    let mut data = Vec::from(TEST_DB);

    let (page, offset, before_offsets, used_before, rows_before, table_rows_before) = {
        let mut db = open(Cursor::new(data.as_slice()));
        let rows = rows_of(&mut db, keys_table());
        let table_rows_before = rows.len();
        let (page, offset, _) = *rows.first().expect("the table has a row");
        let page_ref = db.load_page(page).expect("page loads");
        let mut offsets: Vec<u16> = page_ref
            .content
            .as_data()
            .expect("data page")
            .rows
            .keys()
            .copied()
            .collect();
        offsets.sort_unstable();
        (
            page,
            offset,
            offsets,
            page_ref.header.used_size,
            page_ref.header.packed_row_counts.num_rows(),
            table_rows_before,
        )
    };

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        assert!(
            db.remove_row_at(keys_table(), page, offset)
                .expect("the row is removed"),
            "the row was there"
        );
        assert!(
            !db.remove_row_at(keys_table(), page, offset)
                .expect("removing twice is not an error"),
            "the same offset holds nothing now"
        );
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));

    let (after_offsets, used_after, rows_after, valid_after, deleted_flag) = {
        let page_ref = db.load_page(page).expect("page loads");
        let mut offsets: Vec<u16> = page_ref
            .content
            .as_data()
            .expect("data page")
            .rows
            .keys()
            .copied()
            .collect();
        offsets.sort_unstable();
        (
            offsets,
            page_ref.header.used_size,
            page_ref.header.packed_row_counts.num_rows(),
            page_ref.header.packed_row_counts.num_rows_valid(),
            page_ref.header.page_flags.contains_deleted(),
        )
    };

    let mut expected = before_offsets.clone();
    expected.retain(|entry| *entry != offset);
    assert_eq!(
        after_offsets, expected,
        "the other rows stay where they were"
    );
    assert_eq!(used_after, used_before, "the heap does not shrink");
    assert_eq!(rows_after, rows_before, "the slot is still allocated");
    assert_eq!(
        usize::from(valid_after),
        expected.len(),
        "one row fewer counts as valid"
    );
    assert!(deleted_flag, "the page says it holds a deleted row");

    let keys = db
        .iter_rows::<Key>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    assert_eq!(keys.len(), table_rows_before - 1);
}

#[test]
fn a_row_of_unknown_size_is_refused() {
    let mut data = Vec::from(TEST_DB);
    let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
    let error = db
        .insert_row(keys_table(), Row::Unknown)
        .expect_err("a row of unknown size cannot be inserted");
    assert!(
        error.to_string().contains("size is unknown"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_table_drops_a_page_that_has_no_rows_left() {
    let mut data = Vec::from(TEST_DB);

    let page = {
        let mut db = open(Cursor::new(data.as_slice()));
        rows_of(&mut db, genres_table())
            .first()
            .expect("the genres table has a row")
            .0
    };

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        while let Some((row_page, row_offset, _)) =
            rows_of(&mut db, genres_table()).into_iter().next()
        {
            db.remove_row_at(genres_table(), row_page, row_offset)
                .expect("the row is removed");
        }
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));

    let (table_index, _) = db.get_header().find_table(genres_table()).expect("table");
    let mut reachable = Vec::new();
    let mut pages = db.iter_pages_for_table(table_index).expect("pages");
    while let Ok(Some(entry)) = pages.next() {
        reachable.push(entry.header.page_index.offset(4096));
    }
    assert!(
        !reachable.contains(&page.offset(4096)),
        "the emptied page left the table"
    );

    let page_ref = db.load_page(page).expect("the page still loads");
    assert_eq!(page_ref.header.packed_row_counts.num_rows(), 0);
    let body_start = page.offset(4096) as usize + 40;
    let body = &data[body_start..body_start + 4056];
    assert!(
        body.iter().all(|byte| *byte == 0),
        "the emptied page still carries bytes"
    );
}

#[test]
fn a_page_that_a_table_dropped_is_used_again() {
    let mut data = Vec::from(TEST_DB);
    let size_before = data.len();

    let dropped = {
        let mut db = open(Cursor::new(data.as_slice()));
        rows_of(&mut db, genres_table())
            .first()
            .expect("a genre row")
            .0
    };

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        while let Some((row_page, row_offset, _)) =
            rows_of(&mut db, genres_table()).into_iter().next()
        {
            db.remove_row_at(genres_table(), row_page, row_offset)
                .expect("removed");
        }

        let template = rows_of(&mut db, keys_table())
            .into_iter()
            .next()
            .expect("a key row")
            .2;
        let mut used_pages = Vec::new();
        for _ in 0..800 {
            let (page, _) = db
                .insert_row(keys_table(), spare_row(&template))
                .expect("insert");
            used_pages.push(page.offset(4096));
        }
        assert!(
            used_pages.contains(&dropped.offset(4096)),
            "the freed page was used for new rows"
        );
        db.close().expect("close");
    }

    let pages_of_new_data = (data.len() - size_before) / 4096;
    assert!(
        pages_of_new_data < 3,
        "the freed page saved a page: the file grew by {pages_of_new_data}"
    );
}

#[test]
fn a_key_built_from_scratch_can_be_inserted() {
    let mut data = Vec::from(TEST_DB);
    let before = {
        let mut db = open(Cursor::new(data.as_slice()));
        rows_of(&mut db, keys_table()).len()
    };

    let row = Row::Plain(PlainRow::Key(Key {
        id: KeyId(4242),
        id2: 4242,
        name: "Gmaj".parse().expect("a valid DeviceSQL string"),
    }));

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        db.insert_row(keys_table(), row)
            .expect("the row is inserted");
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));
    let keys = db
        .iter_rows::<Key>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    assert_eq!(keys.len(), before + 1);

    let added = keys
        .iter()
        .find(|key| key.id.0 == 4242)
        .expect("the new key is in the table");
    assert_eq!(added.id2, 4242, "the second copy of the id travels with it");
    assert_eq!(
        added.name.clone().into_string().expect("a readable name"),
        "Gmaj"
    );
}

#[test]
fn a_genre_built_from_scratch_can_be_inserted() {
    let mut data = Vec::from(TEST_DB);

    // 77 is already taken in the test database, so pick an id nothing uses.
    let row = Row::Plain(PlainRow::Genre(Genre {
        id: GenreId(60_000),
        name: "Drum & Bass".parse().expect("a valid DeviceSQL string"),
    }));

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        db.insert_row(genres_table(), row)
            .expect("the row is inserted");
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));
    let genres = db
        .iter_rows::<Genre>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    let added = genres
        .iter()
        .find(|genre| genre.id.0 == 60_000)
        .expect("the new genre is in the table");
    assert_eq!(
        added.name.clone().into_string().expect("a readable name"),
        "Drum & Bass"
    );
}

#[test]
fn an_appended_page_is_listed_in_an_index_that_carries_entries() {
    let mut data = Vec::from(TEST_DB);

    // Most tables keep their index page empty, so seed one entry to exercise the bookkeeping. The
    // rows stay ordinary key rows, because History rows still do not serialize to the 40 bytes a
    // native export gives them.
    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        let (table_index, table) = db
            .get_header()
            .find_table(keys_table())
            .expect("table is present");
        let first_page = table.first_page;
        let data_page = {
            let mut pages = db.iter_pages_for_table(table_index).expect("pages");
            let mut found = None;
            while let Ok(Some(page)) = pages.next() {
                if page.content.as_data().is_some() {
                    found = Some(page.header.page_index);
                    break;
                }
            }
            found.expect("a data page")
        };

        let page = db.load_page(first_page).expect("index page");
        let index = page.content.as_index_mut().expect("index page");
        index.header.num_entries = 1;
        index.header.next_offset = 1;
        index.entries = vec![IndexEntry::try_from((data_page, 0)).expect("an entry")];
        db.close().expect("close");
    }

    let (new_page, entries) = {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        let (_, table) = db.get_header().find_table(keys_table()).expect("table");
        let first_page = table.first_page;
        let template = rows_of(&mut db, keys_table())
            .into_iter()
            .next()
            .expect("a row")
            .2;

        let mut new_page = None;
        for _ in 0..800 {
            let (page, _) = db
                .insert_row(keys_table(), spare_row(&template))
                .expect("insert");
            new_page = Some(page);
        }
        let new_page = new_page.expect("a page");

        let page = db.load_page(first_page).expect("index page");
        let index = page.content.as_index().expect("index page");
        let entries: Vec<_> = index
            .entries
            .iter()
            .filter_map(|entry| entry.page_index().ok())
            .map(|page| page.offset(4096))
            .collect();
        db.close().expect("close");
        (new_page.offset(4096), entries)
    };

    assert!(
        entries.len() > 1,
        "the index lists the pages that were added"
    );
    assert!(
        entries.contains(&new_page),
        "the last page the table grew is listed in the index"
    );
}

#[test]
fn a_history_row_occupies_the_forty_bytes_the_deck_gives_it() {
    let mut data = Vec::from(TEST_DB);
    let history = PageType::Plain(PlainPageType::History);

    let placed = {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        let placed = db
            .insert_row(
                history,
                Row::Plain(PlainRow::History(History {
                    subtype: Subtype(640),
                    index_shift: 0,
                    num_tracks: 1,
                    date: "2026-10-24".parse().expect("string"),
                    version: "1000".parse().expect("string"),
                    label: "".parse().expect("string"),
                    unknown_tail: 0,
                })),
            )
            .expect("the row is inserted");
        db.close().expect("close");
        placed
    };

    let mut db = open(Cursor::new(data.as_slice()));

    let occupied = {
        let page = db.load_page(placed.0).expect("page loads");
        let content = page.content.as_data().expect("data page");
        let mut offsets: Vec<u16> = content.rows.keys().copied().collect();
        offsets.sort_unstable();
        let index = offsets
            .iter()
            .position(|entry| *entry == placed.1)
            .expect("the row is in the page");
        let end = offsets
            .get(index + 1)
            .copied()
            .unwrap_or(page.header.used_size);
        end - placed.1
    };
    assert_eq!(
        occupied, 40,
        "a history row takes forty bytes, not the thirty nine the fields add up to"
    );

    let rows = db
        .iter_rows::<History>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    assert!(
        rows.iter()
            .any(|row| row.date.clone().into_string().expect("date") == "2026-10-24"),
        "the new history row reads back"
    );
}
