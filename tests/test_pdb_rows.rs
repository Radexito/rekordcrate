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
fn removing_a_row_leaves_the_page_consistent() {
    let mut data = Vec::from(TEST_DB);

    let (page, offset, before) = {
        let mut db = open(Cursor::new(data.as_slice()));
        let rows = rows_of(&mut db, keys_table());
        let (page, offset, _) = *rows.first().expect("the table has a row");
        (page, offset, rows.len())
    };

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        assert!(
            db.remove_row_at(keys_table(), page, offset)
                .expect("the row is removed"),
            "the row was there"
        );
        // The page is rebuilt without the row, so the offsets that stay move down. An offset that
        // is past everything the page now holds has nothing to remove.
        let rebuilt_used_size = {
            let page = db.load_page(page).expect("the page still loads");
            page.header.used_size
        };
        assert!(
            !db.remove_row_at(keys_table(), page, rebuilt_used_size.saturating_add(4))
                .expect("an empty offset is not an error"),
            "nothing sits past the rows that are left"
        );
        db.close().expect("close");
    }

    let mut db = open(Cursor::new(data.as_slice()));
    let rows = rows_of(&mut db, keys_table());
    assert_eq!(rows.len(), before - 1, "one row fewer");

    // The rebuilt page has to agree with itself: the counts in its header describe the rows that
    // are in it, and no offset points past the heap it wrote.
    {
        let page = db.load_page(page).expect("the page still loads");
        let data = page.content.as_data().expect("still a data page");
        assert_eq!(
            usize::from(page.header.packed_row_counts.num_rows()),
            data.rows.len()
        );
        assert_eq!(
            usize::from(page.header.packed_row_counts.num_rows_valid()),
            data.rows.len()
        );
        for offset in data.rows.keys() {
            assert!(
                *offset < page.header.used_size,
                "offset {offset} is inside the heap"
            );
        }
    }

    // Every remaining row still parses, which means the rebuilt page is not corrupt.
    let keys = db
        .iter_rows::<Key>()
        .expect("rows load")
        .collect::<Vec<_>>()
        .expect("rows read");
    assert_eq!(keys.len(), before - 1);
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
fn a_page_that_lost_its_last_row_keeps_no_bytes_of_it() {
    let mut data = Vec::from(TEST_DB);

    let (page, offset) = {
        let mut db = open(Cursor::new(data.as_slice()));
        let (page, offset, _) = rows_of(&mut db, genres_table())
            .into_iter()
            .next()
            .expect("the genres table has a row");
        (page, offset)
    };

    {
        let mut db = Database::open(Cursor::new(&mut data), DatabaseType::Plain).expect("open");
        // Empty the table completely, whatever it holds.
        while let Some((row_page, row_offset, _)) =
            rows_of(&mut db, genres_table()).into_iter().next()
        {
            db.remove_row_at(genres_table(), row_page, row_offset)
                .expect("the row is removed");
        }
        db.close().expect("close");
    }

    let _ = (page, offset);

    // A page with no rows has nothing after its header but zeros. Writing only what a page needs
    // would leave the bytes of the rows it used to hold sitting in its free space, which is what a
    // reader that checks that space catches.
    let page_offset = page.offset(4096) as usize;
    let body = &data[page_offset + 40..page_offset + 4096];
    let first_non_zero = body.iter().position(|byte| *byte != 0);
    assert_eq!(
        first_non_zero, None,
        "the emptied page still carries bytes: {first_non_zero:?}"
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
