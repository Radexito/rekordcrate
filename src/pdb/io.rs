// Copyright (c) 2026 Jan Holthuis <jan.holthuis@rub.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy
// of the MPL was not distributed with this file, You can obtain one at
// http://mozilla.org/MPL/2.0/.
//
// SPDX-License-Identifier: MPL-2.0

//! Parser for Pioneer DeviceSQL database exports (PDB).
//!
//! The Rekordbox DJ software uses writes PDB files to `/PIONEER/rekordbox/export.pdb`.
//!
//! Most of the file format has been reverse-engineered by Henry Betts, Fabian Lesniak and James
//! Elliott.
//!
//! - <https://github.com/Deep-Symmetry/crate-digger/blob/master/doc/Analysis.pdf>
//! - <https://djl-analysis.deepsymmetry.org/rekordbox-export-analysis/exports.html>
//! - <https://github.com/henrybetts/Rekordbox-Decoding>
//! - <https://github.com/flesniak/python-prodj-link/tree/master/prodj/pdblib>

use super::*;
use crate::util::{RekordcrateError, RekordcrateResult, TableIndex};
use binrw::{binrw, io::SeekFrom, BinRead, BinResult, BinWrite, Endian};
use fallible_iterator::{FallibleIterator, IteratorExt};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Seek, Write};

/// A lazily loaded PDB database.
#[binrw]
#[brw(little)]
#[br(import(db_type: DatabaseType))]
#[derive(Debug, PartialEq)]
struct LazyDatabase {
    /// The PDB header.
    #[br(args(db_type))]
    #[bw(pad_size_to = header.page_size as usize)]
    header: Header,
    /// The pages of the database, initially not loaded.
    #[br(calc = vec![LazyPage::NotLoaded; header.next_unused_page.0.saturating_sub(1) as usize])]
    #[bw(args(header.page_size))]
    pages: Vec<LazyPage>,
}

#[derive(Debug, PartialEq, Clone)]
enum LazyPage {
    NotLoaded,
    Loaded(Page),
}

impl BinWrite for LazyPage {
    type Args<'a> = (u32,);

    fn write_options<W: Write + Seek>(
        &self,
        writer: &mut W,
        endian: Endian,
        (page_size,): Self::Args<'_>,
    ) -> BinResult<()> {
        match self {
            LazyPage::NotLoaded => {
                // Just seek forward without writing anything.
                writer.seek(SeekFrom::Current(page_size as i64))?;
                Ok(())
            }
            LazyPage::Loaded(page) => page.write_options(writer, endian, (page_size,)),
        }
    }
}

fn read_page<IO: Read + Seek>(
    io: &mut IO,
    page_index: PageIndex,
    page_size: u32,
    db_type: DatabaseType,
) -> RekordcrateResult<Page> {
    let endian = Endian::Little;
    let page_offset = SeekFrom::Start(page_index.offset(page_size));
    io.seek(page_offset).map_err(binrw::Error::Io)?;
    let page = Page::read_options(io, endian, (page_size, db_type))?;
    Ok(page)
}

/// A PDB database opened for reading or writing.
#[derive(Debug)]
pub struct Database<IO> {
    io: IO,
    db_type: DatabaseType,
    content: LazyDatabase,
}

impl<R: Read + Seek> Database<R> {
    /// Opens a PDB database without writing back to disk.
    /// Still allows modifying data in memory.
    pub fn open_non_persistent(mut io: R, db_type: DatabaseType) -> RekordcrateResult<Self> {
        let endian = Endian::Little;
        let content = LazyDatabase::read_options(&mut io, endian, (db_type,))?;
        Ok(Self {
            io,
            db_type,
            content,
        })
    }

    /// Loads a page into memory.
    pub fn load_page(&mut self, index: PageIndex) -> RekordcrateResult<&mut Page> {
        let page_entry = self
            .content
            .pages
            .get_mut(index.0 as usize - 1)
            .ok_or_else(|| RekordcrateError::PageNotPresent(index))?;
        if let LazyPage::NotLoaded = page_entry {
            let page = read_page(
                &mut self.io,
                index,
                self.content.header.page_size,
                self.db_type,
            )?;
            *page_entry = LazyPage::Loaded(page);
        }
        match page_entry {
            LazyPage::Loaded(page) => Ok(page),
            _ => unreachable!(),
        }
    }

    /// Loads all pages for a table into memory and iterates over them.
    pub fn iter_pages_for_table<'db>(
        &'db mut self,
        table_index: TableIndex,
    ) -> RekordcrateResult<PageIterator<'db, R>> {
        let table = self
            .get_header()
            .tables
            .get(table_index.0)
            .ok_or_else(|| RekordcrateError::TableNotPresent(table_index))?;
        let (first_page, last_page) = (table.first_page, table.last_page);

        Ok(PageIterator {
            db_pages: self.content.pages.as_mut_slice(),
            db_pages_offset: 1, // Page indices are 1-based, so the first page is at offset 0 in the slice.
            db_io: &mut self.io,
            db_page_size: self.content.header.page_size,
            db_type: self.db_type,
            next_page: Some(first_page),
            last_page,
        })
    }

    /// Loads all pages for a page type into memory and iterates over them.
    pub fn iter_pages(&mut self, page_type: PageType) -> RekordcrateResult<PageIterator<'_, R>> {
        let (_, table) = self
            .get_header()
            .find_table(page_type)
            .ok_or_else(|| RekordcrateError::TableTypeNotPresent(page_type))?;
        let (first_page, last_page) = (table.first_page, table.last_page);

        Ok(PageIterator {
            db_pages: self.content.pages.as_mut_slice(),
            db_pages_offset: 1, // Page indices are 1-based, so the first page is at offset 0 in the slice.
            db_io: &mut self.io,
            db_page_size: self.content.header.page_size,
            db_type: self.db_type,
            next_page: Some(first_page),
            last_page,
        })
    }

    /// Loads all pages for a page type into memory and iterates over their data rows.
    pub fn iter_rows<'a, RowT: RowVariant + 'a>(
        &'a mut self,
    ) -> RekordcrateResult<impl FallibleIterator<Item = &'a mut RowT, Error = RekordcrateError>>
    {
        Ok(self
            .iter_pages(RowT::PAGE_TYPE)?
            .filter_map(|page| Ok(page.content.as_data_mut()))
            .flat_map(|dpc| {
                Ok(dpc
                    .rows
                    .values_mut()
                    .into_fallible()
                    .map_err(|_: core::convert::Infallible| unreachable!()))
            })
            // The parsed row type is determined from the page type, so if we find an unexpected
            // variant then there is a code bug (not simply a corrupt DB).
            .map(|row| Ok(row.as_variant_mut().expect("unexpected row type"))))
    }

    /// Returns a reference to the PDB header.
    #[must_use]
    pub fn get_header(&self) -> &Header {
        &self.content.header
    }

    /// Returns a mutable reference to the PDB header.
    #[must_use]
    pub fn get_header_mut(&mut self) -> &mut Header {
        &mut self.content.header
    }
}

impl<RW: Read + Write + Seek> Database<RW> {
    /// Opens a PDB database for reading and writing.
    pub fn open(mut io: RW, db_type: DatabaseType) -> RekordcrateResult<Self> {
        let endian = Endian::Little;
        let content = LazyDatabase::read_options(&mut io, endian, (db_type,))?;
        Ok(Self {
            io,
            db_type,
            content,
        })
    }

    /// Flushes all changes to the underlying IO.
    ///
    /// A page is written as a whole block. Writing only the bytes a page needs would leave whatever
    /// the previous version of that page had behind it, so a page that lost a row would keep the old
    /// row's bytes in its free space, and a reader that checks that space is zero would reject the
    /// page. Pages that were never loaded are left untouched instead of being overwritten.
    pub fn flush(&mut self) -> RekordcrateResult<()> {
        let endian = Endian::Little;
        let page_size = self.content.header.page_size;
        let db_type = self.db_type;

        let Self { io, content, .. } = self;
        io.seek(SeekFrom::Start(0))?;

        // The file header occupies the first page, and the pages after it start at a page boundary.
        let mut header = Vec::with_capacity(page_size as usize);
        content
            .header
            .write_options(&mut Cursor::new(&mut header), endian, (db_type,))?;
        header.resize(page_size as usize, 0);
        io.write_all(&header)?;

        for page in &mut content.pages {
            match page {
                LazyPage::NotLoaded => {
                    io.seek(SeekFrom::Current(i64::from(page_size)))?;
                }
                LazyPage::Loaded(page) => {
                    let mut body = Vec::with_capacity(page_size as usize);
                    page.write_options(&mut Cursor::new(&mut body), endian, (page_size,))?;
                    if body.len() > page_size as usize {
                        return Err(RekordcrateError::IntegrityError(
                            "a page grew past the page size of the file",
                        ));
                    }
                    body.resize(page_size as usize, 0);
                    io.write_all(&body)?;
                }
            }
        }

        Ok(())
    }

    /// Closes the database, flushing changes.
    pub fn close(mut self) -> RekordcrateResult<()> {
        self.flush()?;
        Ok(())
    }

    /// Returns the table that holds rows of the given page type.
    fn table_index_of(&self, page_type: PageType) -> RekordcrateResult<TableIndex> {
        self.content
            .header
            .tables
            .iter()
            .position(|table| table.page_type == page_type)
            .map(TableIndex)
            .ok_or(RekordcrateError::IntegrityError(
                "the database has no table for this page type",
            ))
    }

    /// Number of pages that exist in the file, plus one: the first index that is free to use.
    ///
    /// The header's `next_unused_page` cannot answer this. Exports keep a value well past the last
    /// page that is actually in the file, so taking it at face value writes the new page beyond a
    /// hole.
    fn first_free_page(&mut self) -> RekordcrateResult<u32> {
        let page_size = u64::from(self.content.header.page_size);
        let length = self.io.seek(SeekFrom::End(0))?;
        let pages = length.div_ceil(page_size);
        Ok(u32::try_from(pages)
            .map_err(|_| {
                RekordcrateError::IntegrityError("the file has more pages than a u32 holds")
            })?
            .saturating_add(1))
    }

    /// Inserts a row into the table that holds the given page type.
    ///
    /// The row goes into the first data page of that table with room for it. If every page is full,
    /// a data page is added to the table, which is why the file can grow by [`Database::page_size`]
    /// bytes.
    ///
    /// If the table's index page carries entries, the new page is listed there too.
    ///
    /// Returns the page and the offset inside it where the row was placed.
    pub fn insert_row(
        &mut self,
        page_type: PageType,
        row: Row,
    ) -> RekordcrateResult<(PageIndex, u16)> {
        let bytes = row.heap_bytes_required(());
        if bytes == 0 {
            return Err(RekordcrateError::IntegrityError(
                "a row whose size is unknown cannot be inserted",
            ));
        }

        let mut row = Some(row);
        let table_index = self.table_index_of(page_type)?;
        let (first_page, last_page) = {
            let table = &self.content.header.tables[table_index.0];
            (table.first_page, table.last_page)
        };

        // A page that already belongs to the table costs nothing else: no new page, no new link.
        let mut index = first_page;
        loop {
            let (next_page, at_last_page) = {
                let page = self.load_page(index)?;
                let next_page = page.header.next_page;
                let at_last_page = index == last_page;

                if page.content.as_data().is_some() {
                    let offset = page.header.used_size;
                    if let Some(insert) = page.allocate_row(bytes) {
                        insert(row.take().expect("the row is inserted once"));
                        return Ok((index, offset));
                    }
                }

                (next_page, at_last_page)
            };

            if at_last_page {
                break;
            }
            index = next_page;
        }

        // Every page is full. Copy what the last page knows, so the new one looks like its siblings.
        let (next_page, unknown1, data_header) = {
            let previous = self.load_page(last_page)?;
            let data_header = previous.content.as_data().map(|data| data.header.clone());
            (
                previous.header.next_page,
                previous.header.unknown1,
                data_header.unwrap_or(DataPageHeader {
                    unknown5: 1,
                    unknown_not_num_rows_large: 0,
                    unknown6: 0,
                    unknown7: 0,
                }),
            )
        };

        let new_index = PageIndex(self.first_free_page()?);
        let free_size = self.empty_page_free_size()?;
        let mut page = Page {
            header: PageHeader {
                page_index: new_index,
                page_type,
                next_page,
                unknown1,
                unknown2: 0,
                packed_row_counts: PackedRowCounts::new(),
                page_flags: PageFlags::new_data_page(),
                free_size,
                used_size: 0,
            },
            content: PageContent::Data(DataPageContent {
                header: data_header,
                row_groups: Vec::new(),
                rows: BTreeMap::new(),
            }),
        };

        let offset = page.header.used_size;
        let insert = page
            .allocate_row(bytes)
            .ok_or(RekordcrateError::IntegrityError(
                "the row does not fit into an empty page",
            ))?;
        insert(row.take().expect("the row is inserted once"));

        let slot = usize::try_from(new_index.0.saturating_sub(1)).map_err(|_| {
            RekordcrateError::IntegrityError("page index does not fit into a usize")
        })?;
        if slot < self.content.pages.len() {
            self.content.pages[slot] = LazyPage::Loaded(page);
        } else {
            self.content.pages.push(LazyPage::Loaded(page));
        }
        if new_index.0 >= self.content.header.next_unused_page.0 {
            self.content.header.next_unused_page = PageIndex(new_index.0.saturating_add(1));
        }

        // The page that used to be last now leads to the new one, and the table says so too.
        self.load_page(last_page)?.header.next_page = new_index;
        self.content.header.tables[table_index.0].last_page = new_index;

        // A table whose index page carries entries has to list the new page, or a reader that walks
        // that index never sees it. Most tables keep the index empty (827 of 870 in the exports), and
        // those stay empty, the same way the files rekordbox writes look.
        let page_size = self.content.header.page_size;
        {
            let index_page = self.load_page(first_page)?;
            if let Some(index) = index_page.content.as_index_mut() {
                if !index.entries.is_empty()
                    && index.entries.len() < IndexPageContent::total_entries(page_size)
                {
                    let flags = index.entries.last().map_or(0, |entry| entry.index_flags());
                    if let Ok(entry) = IndexEntry::try_from((new_index, flags)) {
                        index.entries.push(entry);
                        if let Ok(count) = u16::try_from(index.entries.len()) {
                            index.header.num_entries = count;
                        }
                        index.header.next_offset = index.header.next_offset.saturating_add(1);
                    }
                }
            }
        }

        Ok((new_index, offset))
    }

    /// Removes the row that sits at the given offset in a data page of the given page type.
    ///
    /// The page is rebuilt from the rows that stay, so the offsets, the row groups and the counts in
    /// its header describe what is actually there afterwards. Rebuilding packs the remaining rows
    /// densely, which means the offsets of the other rows in that page move: locate them again before
    /// removing another one. Returns `false` if the offset holds no row.
    pub fn remove_row_at(
        &mut self,
        page_type: PageType,
        page_index: PageIndex,
        offset: u16,
    ) -> RekordcrateResult<bool> {
        let (rows, data_header, next_page, unknown1) = {
            let page = self.load_page(page_index)?;
            if page.header.page_type != page_type {
                return Err(RekordcrateError::IntegrityError(
                    "the page holds rows of another type",
                ));
            }
            let Some(data) = page.content.as_data() else {
                return Err(RekordcrateError::IntegrityError(
                    "rows live in data pages, not in index pages",
                ));
            };
            let mut rows: Vec<(u16, Row)> = data
                .rows
                .iter()
                .map(|(offset, row)| (*offset, row.clone()))
                .collect();
            let Some(position) = rows.iter().position(|(key, _)| *key == offset) else {
                return Ok(false);
            };
            rows.remove(position);
            (
                rows.into_iter().map(|(_, row)| row).collect::<Vec<_>>(),
                data.header.clone(),
                page.header.next_page,
                page.header.unknown1,
            )
        };

        let mut page = Page {
            header: PageHeader {
                page_index,
                page_type,
                next_page,
                unknown1,
                unknown2: 0,
                packed_row_counts: PackedRowCounts::new(),
                page_flags: PageFlags::new_data_page(),
                free_size: self.empty_page_free_size()?,
                used_size: 0,
            },
            content: PageContent::Data(DataPageContent {
                header: data_header,
                row_groups: Vec::new(),
                rows: BTreeMap::new(),
            }),
        };
        for row in rows {
            let bytes = row.heap_bytes_required(());
            let insert = page
                .allocate_row(bytes)
                .ok_or(RekordcrateError::IntegrityError(
                    "the remaining rows do not fit into their page",
                ))?;
            insert(row);
        }

        let slot = usize::try_from(page_index.0.saturating_sub(1)).map_err(|_| {
            RekordcrateError::IntegrityError("page index does not fit into a usize")
        })?;
        if slot >= self.content.pages.len() {
            return Err(RekordcrateError::PageNotPresent(page_index));
        }
        self.content.pages[slot] = LazyPage::Loaded(page);
        Ok(true)
    }

    /// The free space a data page with no rows has.
    fn empty_page_free_size(&self) -> RekordcrateResult<u16> {
        let page_size = self.content.header.page_size;
        u16::try_from(page_size - PageHeader::BINARY_SIZE - DataPageHeader::BINARY_SIZE).map_err(
            |_| RekordcrateError::IntegrityError("the page is too large to describe in 16 bits"),
        )
    }
}

/// An iterator over pages in a PDB database.
///
/// We use `FallibleIterator` instead of the standard `Iterator` trait
/// to improve the ergonomics of error handling while loading pages.
///
/// # Usage
///
/// ```no_run
/// # use rekordcrate::pdb::*;
/// # use rekordcrate::util::RekordcrateError;
/// # use rekordcrate::pdb::io::Database;
/// use fallible_iterator::FallibleIterator;
///
/// # let mut db: Database<std::fs::File> = unimplemented!();
/// // Loop over pages.
/// let mut page_iter = db.iter_pages(PageType::Plain(PlainPageType::Tracks))?;
/// while let Some(page) = page_iter.next()? {
///     // Process the page
/// }
///
/// // Iterate over pages using typical functional combinators.
/// // Note that combinators like `map` should return a `Result`.
/// let results: Vec<_> = db
///     .iter_pages(PageType::Plain(PlainPageType::Tracks))?
///     .map(|page| Ok(todo!()))
///     .collect()?;
/// # Ok::<(), RekordcrateError>(())
/// ```
#[derive(Debug)]
pub struct PageIterator<'db, IO> {
    db_pages: &'db mut [LazyPage],
    db_pages_offset: usize,
    db_io: &'db mut IO,
    db_page_size: u32,
    db_type: DatabaseType,

    next_page: Option<PageIndex>,
    last_page: PageIndex,
}

impl<'db, R: Read + Seek> FallibleIterator for PageIterator<'db, R> {
    type Item = &'db mut Page;
    type Error = RekordcrateError;

    /// Loads the next page in the iterator.
    fn next(&mut self) -> RekordcrateResult<Option<&'db mut Page>> {
        match self.next_page {
            None => Ok(None),
            Some(page_index) => {
                // Throw away references to pages lower than the next page index,
                // leaving our target page at the start of `pages`.
                // ASSUMPTION: pages in a table are linked in increasing order by index.
                let slice_index = (page_index.0 as usize)
                    .checked_sub(self.db_pages_offset)
                    .ok_or(RekordcrateError::PageOrderViolation(page_index))?;
                let db_pages: &'db mut [LazyPage] = std::mem::take(&mut self.db_pages);
                let (_, pages): (_, &'db mut [LazyPage]) = db_pages
                    .split_at_mut_checked(slice_index)
                    .ok_or(RekordcrateError::PageNotPresent(page_index))?;
                // Pull out the target page and leave the rest in `self.db_pages`.
                let (page_entry, pages): (&'db mut LazyPage, &'db mut [LazyPage]) = pages
                    .split_first_mut()
                    .ok_or(RekordcrateError::PageNotPresent(page_index))?;
                self.db_pages = pages;
                self.db_pages_offset = page_index.0 as usize + 1;

                if let LazyPage::NotLoaded = page_entry {
                    let page = read_page(self.db_io, page_index, self.db_page_size, self.db_type)?;
                    *page_entry = LazyPage::Loaded(page);
                }
                let page: &'db mut Page = match page_entry {
                    LazyPage::Loaded(page) => page,
                    _ => unreachable!(),
                };

                if page_index == self.last_page {
                    self.next_page = None;
                } else {
                    self.next_page = Some(page.header.next_page);
                }
                Ok(Some(page))
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::fs::File;

    #[test]
    fn test_pageiterator_safety() {
        // This was written when PageIterator used unsafe.
        // It's a small test and provides value in case we ever want to use unsafe again.
        // Run with `MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test test_pageiterator_safety`.
        let file = File::open("data/pdb/num_rows/export.pdb").unwrap();
        let mut db = Database::open_non_persistent(file, DatabaseType::Plain).unwrap();
        let mut iter = db
            .iter_pages(PageType::Plain(PlainPageType::Tracks))
            .unwrap();

        let first = iter.next().unwrap().unwrap();
        let second = iter.next().unwrap().unwrap();

        // Should be disallowed since `db` is still borrowed by `iter` until all pages go out of scope.
        // let _iter2 = db
        //     .iter_pages(PageType::Plain(PlainPageType::Tracks))
        //     .unwrap();

        assert_eq!(
            first.header.page_type,
            PageType::Plain(PlainPageType::Tracks)
        );
        assert_eq!(
            second.header.page_type,
            PageType::Plain(PlainPageType::Tracks)
        );

        // Should be allowed since the `db` borrow can now be released.
        let _iter3 = db
            .iter_pages(PageType::Plain(PlainPageType::Tracks))
            .unwrap();
    }
}
