// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! fork from https://github.com/apache/arrow-rs/blob/5.2.0/parquet/src/file/serialized_reader.rs

//! Contains implementations of the reader traits FileReader, RowGroupReader and
//! PageReader Also contains implementations of the ChunkReader for files (with
//! buffering) and byte arrays (RAM)

use std::{fs::File, io::Read, option::Option::Some, sync::Arc};

use arrow_deps::parquet::{
    basic::{Compression, Encoding, Type},
    column::page::{Page, PageReader},
    compression::{create_codec, Codec},
    errors::{ParquetError, Result},
    file::{footer, metadata::*, reader::*, statistics},
    record::{reader::RowIter, Row},
    schema::types::Type as SchemaType,
    util::{cursor::SliceableCursor, memory::ByteBufferPtr},
};
use parquet_format::{PageHeader, PageType};
use thrift::protocol::TCompactInputProtocol;

use crate::{DataCacheRef, MetaCacheRef};

fn format_page_data_key(name: &str, col_start: u64, col_length: u64) -> String {
    format!("{}_{}_{}", name, col_start, col_length)
}

/// Conversion into a [`RowIter`](crate::record::reader::RowIter)
/// using the full file schema over all row groups.
impl IntoIterator for CachableSerializedFileReader<File> {
    type IntoIter = RowIter<'static>;
    type Item = Row;

    fn into_iter(self) -> Self::IntoIter {
        RowIter::from_file_into(Box::new(self))
    }
}

// ----------------------------------------------------------------------
// Implementations of file & row group readers

/// A serialized with cache implementation for Parquet [`FileReader`].
/// Two kinds of items are cacheable:
///  - [`ParquetMetaData`]: only used for creating the reader.
///  - Column chunk bytes: used for reading data by
///    [`SerializedRowGroupReader`].
///
/// Note: the implementation is based on the https://github.com/apache/arrow-rs/blob/5.2.0/parquet/src/file/serialized_reader.rs.
pub struct CachableSerializedFileReader<R: ChunkReader> {
    name: String,
    chunk_reader: Arc<R>,
    metadata: Arc<ParquetMetaData>,
    data_cache: Option<DataCacheRef>,
}

impl<R: 'static + ChunkReader> CachableSerializedFileReader<R> {
    /// Creates file reader from a Parquet file.
    /// Returns error if Parquet file does not exist or is corrupt.
    pub fn new(
        name: String,
        chunk_reader: R,
        meta_cache: Option<MetaCacheRef>,
        data_cache: Option<DataCacheRef>,
    ) -> Result<Self> {
        // MODIFICATION START: consider cache for meta data.
        let metadata = if let Some(meta_cache) = meta_cache {
            if let Some(v) = meta_cache.get(&name) {
                v
            } else {
                let meta_data = Arc::new(footer::parse_metadata(&chunk_reader)?);
                meta_cache.put(name.clone(), meta_data.clone());
                meta_data
            }
        } else {
            Arc::new(footer::parse_metadata(&chunk_reader)?)
        };
        // MODIFICATION END.

        Ok(Self {
            name,
            chunk_reader: Arc::new(chunk_reader),
            metadata,
            data_cache,
        })
    }

    /// Filters row group metadata to only those row groups,
    /// for which the predicate function returns true
    pub fn filter_row_groups(&mut self, predicate: &dyn Fn(&RowGroupMetaData, usize) -> bool) {
        let mut filtered_row_groups = Vec::<RowGroupMetaData>::new();
        for (i, row_group_metadata) in self.metadata.row_groups().iter().enumerate() {
            if predicate(row_group_metadata, i) {
                filtered_row_groups.push(row_group_metadata.clone());
            }
        }
        self.metadata = Arc::new(ParquetMetaData::new(
            self.metadata.file_metadata().clone(),
            filtered_row_groups,
        ));
    }
}

impl<R: 'static + ChunkReader> FileReader for CachableSerializedFileReader<R> {
    fn metadata(&self) -> &ParquetMetaData {
        &self.metadata
    }

    fn num_row_groups(&self) -> usize {
        self.metadata.num_row_groups()
    }

    fn get_row_group(&self, i: usize) -> Result<Box<dyn RowGroupReader + '_>> {
        let row_group_metadata = self.metadata.row_group(i);
        // Row groups should be processed sequentially.
        let f = Arc::clone(&self.chunk_reader);
        Ok(Box::new(SerializedRowGroupReader::new(
            f,
            row_group_metadata,
            self.name.clone(),
            self.data_cache.clone(),
        )))
    }

    fn get_row_iter(&self, projection: Option<SchemaType>) -> Result<RowIter> {
        RowIter::from_file(projection, self)
    }
}

/// A serialized with cache implementation for Parquet [`RowGroupReader`].
///
/// The cache is used for column data chunk when building [`PageReader`].
///
/// NOTE: the implementation is based on the https://github.com/apache/arrow-rs/blob/5.2.0/parquet/src/file/serialized_reader.rs
pub struct SerializedRowGroupReader<'a, R: ChunkReader> {
    chunk_reader: Arc<R>,
    metadata: &'a RowGroupMetaData,
    name: String,
    data_cache: Option<DataCacheRef>,
}

impl<'a, R: ChunkReader> SerializedRowGroupReader<'a, R> {
    /// Creates new row group reader from a file and row group metadata.
    fn new(
        chunk_reader: Arc<R>,
        metadata: &'a RowGroupMetaData,
        name: String,
        data_cache: Option<DataCacheRef>,
    ) -> Self {
        Self {
            chunk_reader,
            metadata,
            name,
            data_cache,
        }
    }

    fn get_data(&self, col_start: u64, col_length: u64) -> Result<Vec<u8>> {
        let mut file_chunk = self.chunk_reader.get_read(col_start, col_length as usize)?;
        let mut buf = Vec::with_capacity(col_length as usize);
        file_chunk.read_to_end(&mut buf).unwrap();
        Ok(buf)
    }

    fn get_file_chunk(&self, col_start: u64, col_length: u64) -> Result<impl Read> {
        if let Some(data_cache) = &self.data_cache {
            let key = format_page_data_key(&self.name, col_start, col_length);
            if let Some(v) = data_cache.get(&key) {
                Ok(SliceableCursor::new(v))
            } else {
                let buf_arc = Arc::new(self.get_data(col_start, col_length)?);
                data_cache.put(key, buf_arc.clone());
                let slice = SliceableCursor::new(buf_arc);
                Ok(slice)
            }
        } else {
            let buf_arc = Arc::new(self.get_data(col_start, col_length)?);
            let slice = SliceableCursor::new(buf_arc);
            Ok(slice)
        }
    }
}

impl<'a, R: 'static + ChunkReader> RowGroupReader for SerializedRowGroupReader<'a, R> {
    fn metadata(&self) -> &RowGroupMetaData {
        self.metadata
    }

    fn num_columns(&self) -> usize {
        self.metadata.num_columns()
    }

    // TODO: fix PARQUET-816
    fn get_column_page_reader(&self, i: usize) -> Result<Box<dyn PageReader>> {
        let col = self.metadata.column(i);
        let (col_start, col_length) = col.byte_range();

        // MODIFICATION START: consider the cache for the data chunk: [col_start,
        // col_start+col_length).
        let file_chunk = self.get_file_chunk(col_start, col_length)?;
        // MODIFICATION END.

        let page_reader = SerializedPageReader::new(
            file_chunk,
            col.num_values(),
            col.compression(),
            col.column_descr().physical_type(),
        )?;
        Ok(Box::new(page_reader))
    }

    fn get_row_iter(&self, projection: Option<SchemaType>) -> Result<RowIter> {
        RowIter::from_row_group(projection, self)
    }
}

/// A serialized implementation for Parquet [`PageReader`].
pub struct SerializedPageReader<T: Read> {
    // The file source buffer which references exactly the bytes for the column trunk
    // to be read by this page reader.
    buf: T,

    // The compression codec for this column chunk. Only set for non-PLAIN codec.
    decompressor: Option<Box<dyn Codec>>,

    // The number of values we have seen so far.
    seen_num_values: i64,

    // The number of total values in this column chunk.
    total_num_values: i64,

    // Column chunk type.
    physical_type: Type,
}

impl<T: Read> SerializedPageReader<T> {
    /// Creates a new serialized page reader from file source.
    pub fn new(
        buf: T,
        total_num_values: i64,
        compression: Compression,
        physical_type: Type,
    ) -> Result<Self> {
        let decompressor = create_codec(compression)?;
        let result = Self {
            buf,
            total_num_values,
            seen_num_values: 0,
            decompressor,
            physical_type,
        };
        Ok(result)
    }

    /// Reads Page header from Thrift.
    fn read_page_header(&mut self) -> Result<PageHeader> {
        let mut prot = TCompactInputProtocol::new(&mut self.buf);
        let page_header = PageHeader::read_from_in_protocol(&mut prot)?;
        Ok(page_header)
    }
}

impl<T: Read> Iterator for SerializedPageReader<T> {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl<T: Read> PageReader for SerializedPageReader<T> {
    fn get_next_page(&mut self) -> Result<Option<Page>> {
        while self.seen_num_values < self.total_num_values {
            let page_header = self.read_page_header()?;

            // When processing data page v2, depending on enabled compression for the
            // page, we should account for uncompressed data ('offset') of
            // repetition and definition levels.
            //
            // We always use 0 offset for other pages other than v2, `true` flag means
            // that compression will be applied if decompressor is defined
            let mut offset: usize = 0;
            let mut can_decompress = true;

            if let Some(ref header_v2) = page_header.data_page_header_v2 {
                offset = (header_v2.definition_levels_byte_length
                    + header_v2.repetition_levels_byte_length) as usize;
                // When is_compressed flag is missing the page is considered compressed
                can_decompress = header_v2.is_compressed.unwrap_or(true);
            }

            let compressed_len = page_header.compressed_page_size as usize - offset;
            let uncompressed_len = page_header.uncompressed_page_size as usize - offset;
            // We still need to read all bytes from buffered stream
            let mut buffer = vec![0; offset + compressed_len];
            self.buf.read_exact(&mut buffer)?;

            // TODO: page header could be huge because of statistics. We should set a
            //  maximum page header size and abort if that is exceeded.
            if let Some(decompressor) = self.decompressor.as_mut() {
                if can_decompress {
                    let mut decompressed_buffer = Vec::with_capacity(uncompressed_len);
                    let decompressed_size =
                        decompressor.decompress(&buffer[offset..], &mut decompressed_buffer)?;
                    if decompressed_size != uncompressed_len {
                        return Err(ParquetError::General(format!(
                            "Actual decompressed size doesn't match the expected one ({} vs {})",
                            decompressed_size, uncompressed_len,
                        )));
                    }
                    if offset == 0 {
                        buffer = decompressed_buffer;
                    } else {
                        // Prepend saved offsets to the buffer
                        buffer.truncate(offset);
                        buffer.append(&mut decompressed_buffer);
                    }
                }
            }

            let result = match page_header.type_ {
                PageType::DictionaryPage => {
                    assert!(page_header.dictionary_page_header.is_some());
                    let dict_header = page_header.dictionary_page_header.as_ref().unwrap();
                    let is_sorted = dict_header.is_sorted.unwrap_or(false);
                    Page::DictionaryPage {
                        buf: ByteBufferPtr::new(buffer),
                        num_values: dict_header.num_values as u32,
                        encoding: Encoding::from(dict_header.encoding),
                        is_sorted,
                    }
                }
                PageType::DataPage => {
                    assert!(page_header.data_page_header.is_some());
                    let header = page_header.data_page_header.unwrap();
                    self.seen_num_values += header.num_values as i64;
                    Page::DataPage {
                        buf: ByteBufferPtr::new(buffer),
                        num_values: header.num_values as u32,
                        encoding: Encoding::from(header.encoding),
                        def_level_encoding: Encoding::from(header.definition_level_encoding),
                        rep_level_encoding: Encoding::from(header.repetition_level_encoding),
                        statistics: statistics::from_thrift(self.physical_type, header.statistics),
                    }
                }
                PageType::DataPageV2 => {
                    assert!(page_header.data_page_header_v2.is_some());
                    let header = page_header.data_page_header_v2.unwrap();
                    let is_compressed = header.is_compressed.unwrap_or(true);
                    self.seen_num_values += header.num_values as i64;
                    Page::DataPageV2 {
                        buf: ByteBufferPtr::new(buffer),
                        num_values: header.num_values as u32,
                        encoding: Encoding::from(header.encoding),
                        num_nulls: header.num_nulls as u32,
                        num_rows: header.num_rows as u32,
                        def_levels_byte_len: header.definition_levels_byte_length as u32,
                        rep_levels_byte_len: header.repetition_levels_byte_length as u32,
                        is_compressed,
                        statistics: statistics::from_thrift(self.physical_type, header.statistics),
                    }
                }
                _ => {
                    // For unknown page type (e.g., INDEX_PAGE), skip and read next.
                    continue;
                }
            };
            return Ok(Some(result));
        }

        // We are at the end of this column chunk and no more page left. Return None.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_deps::parquet::basic::ColumnOrder;

    use super::*;
    use crate::cache::{LruDataCache, LruMetaCache};

    #[test]
    fn test_cursor_and_file_has_the_same_behaviour() {
        let mut buf: Vec<u8> = Vec::new();
        crate::tests::get_test_file("alltypes_plain.parquet")
            .read_to_end(&mut buf)
            .unwrap();
        let cursor = SliceableCursor::new(buf);
        let read_from_cursor =
            CachableSerializedFileReader::new("read_from_cursor".to_string(), cursor, None, None)
                .unwrap();

        let test_file = crate::tests::get_test_file("alltypes_plain.parquet");
        let read_from_file =
            CachableSerializedFileReader::new("read_from_file".to_string(), test_file, None, None)
                .unwrap();

        let file_iter = read_from_file.get_row_iter(None).unwrap();
        let cursor_iter = read_from_cursor.get_row_iter(None).unwrap();

        assert!(file_iter.eq(cursor_iter));
    }

    #[test]
    fn test_reuse_file_chunk() {
        // This test covers the case of maintaining the correct start position in a file
        // stream for each column reader after initializing and moving to the next one
        // (without necessarily reading the entire column).
        let test_file = crate::tests::get_test_file("alltypes_plain.parquet");
        let reader =
            CachableSerializedFileReader::new("test".to_string(), test_file, None, None).unwrap();
        let row_group = reader.get_row_group(0).unwrap();

        let mut page_readers = Vec::new();
        for i in 0..row_group.num_columns() {
            page_readers.push(row_group.get_column_page_reader(i).unwrap());
        }

        // Now buffer each col reader, we do not expect any failures like:
        // General("underlying Thrift error: end of file")
        for mut page_reader in page_readers {
            assert!(page_reader.get_next_page().is_ok());
        }
    }

    fn new_filer_reader_with_cache() -> CachableSerializedFileReader<File> {
        let data_cache: Option<DataCacheRef> = Some(Arc::new(LruDataCache::new(1000)));
        let meta_cache: Option<MetaCacheRef> = Some(Arc::new(LruMetaCache::new(1000)));
        let test_file = crate::tests::get_test_file("alltypes_plain.parquet");
        let reader_result = CachableSerializedFileReader::new(
            "test".to_string(),
            test_file,
            meta_cache.clone(),
            data_cache.clone(),
        );
        assert!(reader_result.is_ok());
        reader_result.unwrap()
    }

    fn test_with_file_reader(reader: &CachableSerializedFileReader<File>) {
        // Test contents in Parquet metadata
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // Test contents in file metadata
        let file_metadata = metadata.file_metadata();
        assert!(file_metadata.created_by().is_some());
        assert_eq!(
            file_metadata.created_by().as_ref().unwrap(),
            "impala version 1.3.0-INTERNAL (build 8a48ddb1eff84592b3fc06bc6f51ec120e1fffc9)"
        );
        assert!(file_metadata.key_value_metadata().is_none());
        assert_eq!(file_metadata.num_rows(), 8);
        assert_eq!(file_metadata.version(), 1);
        assert_eq!(file_metadata.column_orders(), None);

        // Test contents in row group metadata
        let row_group_metadata = metadata.row_group(0);
        assert_eq!(row_group_metadata.num_columns(), 11);
        assert_eq!(row_group_metadata.num_rows(), 8);
        assert_eq!(row_group_metadata.total_byte_size(), 671);
        // Check each column order
        for i in 0..row_group_metadata.num_columns() {
            assert_eq!(file_metadata.column_order(i), ColumnOrder::UNDEFINED);
        }

        // Test row group reader
        let row_group_reader_result = reader.get_row_group(0);
        assert!(row_group_reader_result.is_ok());
        let row_group_reader: Box<dyn RowGroupReader> = row_group_reader_result.unwrap();
        assert_eq!(
            row_group_reader.num_columns(),
            row_group_metadata.num_columns()
        );
        assert_eq!(
            row_group_reader.metadata().total_byte_size(),
            row_group_metadata.total_byte_size()
        );

        // Test page readers
        // TODO: test for every column
        let page_reader_0_result = row_group_reader.get_column_page_reader(0);
        assert!(page_reader_0_result.is_ok());
        let mut page_reader_0: Box<dyn PageReader> = page_reader_0_result.unwrap();
        let mut page_count = 0;
        while let Ok(Some(page)) = page_reader_0.get_next_page() {
            let is_expected_page = match page {
                Page::DictionaryPage {
                    buf,
                    num_values,
                    encoding,
                    is_sorted,
                } => {
                    assert_eq!(buf.len(), 32);
                    assert_eq!(num_values, 8);
                    assert_eq!(encoding, Encoding::PLAIN_DICTIONARY);
                    assert!(!is_sorted);
                    true
                }
                Page::DataPage {
                    buf,
                    num_values,
                    encoding,
                    def_level_encoding,
                    rep_level_encoding,
                    statistics,
                } => {
                    assert_eq!(buf.len(), 11);
                    assert_eq!(num_values, 8);
                    assert_eq!(encoding, Encoding::PLAIN_DICTIONARY);
                    assert_eq!(def_level_encoding, Encoding::RLE);
                    assert_eq!(rep_level_encoding, Encoding::BIT_PACKED);
                    assert!(statistics.is_none());
                    true
                }
                _ => false,
            };
            assert!(is_expected_page);
            page_count += 1;
        }
        assert_eq!(page_count, 2);
    }

    #[test]
    fn test_file_reader() {
        let test_file = crate::tests::get_test_file("alltypes_plain.parquet");
        let reader = CachableSerializedFileReader::new("test".to_string(), test_file, None, None)
            .expect("Should succeed to build test reader");
        test_with_file_reader(&reader);
    }

    #[test]
    fn test_file_reader_with_cache() {
        let reader = new_filer_reader_with_cache();
        let test_num = 10usize;
        for _ in 0..test_num {
            test_with_file_reader(&reader);
        }
    }

    #[test]
    fn test_file_reader_datapage_v2() {
        let test_file = crate::tests::get_test_file("datapage_v2.snappy.parquet");
        let reader_result =
            CachableSerializedFileReader::new("test".to_string(), test_file, None, None);
        assert!(reader_result.is_ok());
        let reader = reader_result.unwrap();

        // Test contents in Parquet metadata
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // Test contents in file metadata
        let file_metadata = metadata.file_metadata();
        assert!(file_metadata.created_by().is_some());
        assert_eq!(
            file_metadata.created_by().as_ref().unwrap(),
            "parquet-mr version 1.8.1 (build 4aba4dae7bb0d4edbcf7923ae1339f28fd3f7fcf)"
        );
        assert!(file_metadata.key_value_metadata().is_some());
        assert_eq!(
            file_metadata.key_value_metadata().to_owned().unwrap().len(),
            1
        );

        assert_eq!(file_metadata.num_rows(), 5);
        assert_eq!(file_metadata.version(), 1);
        assert_eq!(file_metadata.column_orders(), None);

        let row_group_metadata = metadata.row_group(0);

        // Check each column order
        for i in 0..row_group_metadata.num_columns() {
            assert_eq!(file_metadata.column_order(i), ColumnOrder::UNDEFINED);
        }

        // Test row group reader
        let row_group_reader_result = reader.get_row_group(0);
        assert!(row_group_reader_result.is_ok());
        let row_group_reader: Box<dyn RowGroupReader> = row_group_reader_result.unwrap();
        assert_eq!(
            row_group_reader.num_columns(),
            row_group_metadata.num_columns()
        );
        assert_eq!(
            row_group_reader.metadata().total_byte_size(),
            row_group_metadata.total_byte_size()
        );

        // Test page readers
        // TODO: test for every column
        let page_reader_0_result = row_group_reader.get_column_page_reader(0);
        assert!(page_reader_0_result.is_ok());
        let mut page_reader_0: Box<dyn PageReader> = page_reader_0_result.unwrap();
        let mut page_count = 0;
        while let Ok(Some(page)) = page_reader_0.get_next_page() {
            let is_expected_page = match page {
                Page::DictionaryPage {
                    buf,
                    num_values,
                    encoding,
                    is_sorted,
                } => {
                    assert_eq!(buf.len(), 7);
                    assert_eq!(num_values, 1);
                    assert_eq!(encoding, Encoding::PLAIN);
                    assert!(!is_sorted);
                    true
                }
                Page::DataPageV2 {
                    buf,
                    num_values,
                    encoding,
                    num_nulls,
                    num_rows,
                    def_levels_byte_len,
                    rep_levels_byte_len,
                    is_compressed,
                    statistics,
                } => {
                    assert_eq!(buf.len(), 4);
                    assert_eq!(num_values, 5);
                    assert_eq!(encoding, Encoding::RLE_DICTIONARY);
                    assert_eq!(num_nulls, 1);
                    assert_eq!(num_rows, 5);
                    assert_eq!(def_levels_byte_len, 2);
                    assert_eq!(rep_levels_byte_len, 0);
                    assert!(is_compressed);
                    assert!(statistics.is_some());
                    true
                }
                _ => false,
            };
            assert!(is_expected_page);
            page_count += 1;
        }
        assert_eq!(page_count, 2);
    }

    #[test]
    fn test_page_iterator() {
        let file = crate::tests::get_test_file("alltypes_plain.parquet");
        let file_reader = Arc::new(
            CachableSerializedFileReader::new("test".to_string(), file, None, None).unwrap(),
        );

        let mut page_iterator = FilePageIterator::new(0, file_reader.clone()).unwrap();

        // read first page
        let page = page_iterator.next();
        assert!(page.is_some());
        assert!(page.unwrap().is_ok());

        // reach end of file
        let page = page_iterator.next();
        assert!(page.is_none());

        let row_group_indices = Box::new(0..1);
        let mut page_iterator =
            FilePageIterator::with_row_groups(0, row_group_indices, file_reader).unwrap();

        // read first page
        let page = page_iterator.next();
        assert!(page.is_some());
        assert!(page.unwrap().is_ok());

        // reach end of file
        let page = page_iterator.next();
        assert!(page.is_none());
    }

    #[test]
    fn test_file_reader_key_value_metadata() {
        let file = crate::tests::get_test_file("binary.parquet");
        let file_reader = Arc::new(
            CachableSerializedFileReader::new("test".to_string(), file, None, None).unwrap(),
        );

        let metadata = file_reader
            .metadata
            .file_metadata()
            .key_value_metadata()
            .as_ref()
            .unwrap();

        assert_eq!(metadata.len(), 3);

        assert_eq!(metadata.get(0).unwrap().key, "parquet.proto.descriptor");

        assert_eq!(metadata.get(1).unwrap().key, "writer.model.name");
        assert_eq!(metadata.get(1).unwrap().value, Some("protobuf".to_owned()));

        assert_eq!(metadata.get(2).unwrap().key, "parquet.proto.class");
        assert_eq!(
            metadata.get(2).unwrap().value,
            Some("foo.baz.Foobaz$Event".to_owned())
        );
    }

    #[test]
    fn test_file_reader_filter_row_groups() -> Result<()> {
        let test_file = crate::tests::get_test_file("alltypes_plain.parquet");
        let mut reader =
            CachableSerializedFileReader::new("test".to_string(), test_file, None, None)?;

        // test initial number of row groups
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 1);

        // test filtering out all row groups
        reader.filter_row_groups(&|_, _| false);
        let metadata = reader.metadata();
        assert_eq!(metadata.num_row_groups(), 0);

        Ok(())
    }
}
