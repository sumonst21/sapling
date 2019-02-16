// Copyright 2018 Facebook, Inc.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::{
    collections::HashMap,
    fs::File,
    io::{Cursor, Read, Write},
    path::Path,
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use failure::{Fail, Fallible};
use memmap::{Mmap, MmapOptions};

use types::Node;

use crate::error::KeyError;
use crate::fanouttable::FanoutTable;
use crate::sliceext::SliceExt;

const ENTRY_LEN: usize = 40;
const SMALL_FANOUT_CUTOFF: usize = 8192; // 2^16 / 8

#[derive(Debug, Fail)]
#[fail(display = "DataIndex Error: {:?}", _0)]
struct DataIndexError(String);

#[derive(Debug, PartialEq)]
struct DataIndexOptions {
    version: u8,
    // Indicates whether to use the large fanout (2 bytes) or the small (1 byte)
    large: bool,
}

#[derive(Debug)]
pub struct DeltaLocation {
    pub delta_base: Option<Node>,
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug)]
pub struct IndexEntry {
    node: Node,
    delta_base_offset: u32,
    pack_entry_offset: u64,
    pack_entry_size: u64,
}

impl IndexEntry {
    pub fn new(
        node: Node,
        delta_base_offset: DeltaBaseOffset,
        pack_entry_offset: u64,
        pack_entry_size: u64,
    ) -> Self {
        IndexEntry {
            node,
            delta_base_offset: match delta_base_offset {
                DeltaBaseOffset::FullText => 0xffffffff,
                DeltaBaseOffset::Missing => 0xfffffffe,
                DeltaBaseOffset::Offset(value) => value,
            },
            pack_entry_offset,
            pack_entry_size,
        }
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    pub fn delta_base_offset(&self) -> DeltaBaseOffset {
        if self.delta_base_offset == 0xffffffff {
            DeltaBaseOffset::FullText
        } else if self.delta_base_offset == 0xfffffffe {
            DeltaBaseOffset::Missing
        } else {
            DeltaBaseOffset::Offset(self.delta_base_offset.clone())
        }
    }

    pub fn pack_entry_offset(&self) -> u64 {
        self.pack_entry_offset.clone()
    }

    pub fn pack_entry_size(&self) -> u64 {
        self.pack_entry_size.clone()
    }
}

#[derive(Debug, PartialEq)]
pub enum DeltaBaseOffset {
    Offset(u32),
    FullText,
    Missing,
}

impl DeltaBaseOffset {
    fn new(value: i32) -> Fallible<Self> {
        if value >= 0 {
            Ok(DeltaBaseOffset::Offset(value as u32))
        } else if value == -1 {
            Ok(DeltaBaseOffset::FullText)
        } else if value == -2 {
            Ok(DeltaBaseOffset::Missing)
        } else {
            Err(DataIndexError(format!("invalid delta base offset value '{:?}'", value)).into())
        }
    }

    fn to_i32(&self) -> i32 {
        match self {
            &DeltaBaseOffset::Offset(value) => value as i32,
            &DeltaBaseOffset::FullText => -1,
            &DeltaBaseOffset::Missing => -2,
        }
    }
}

impl IndexEntry {
    pub fn read(buf: &[u8]) -> Fallible<Self> {
        let mut cur = Cursor::new(buf);
        cur.set_position(20);
        let node_slice: &[u8] = buf.get_err(0..20)?;
        let node = Node::from_slice(node_slice)?;
        let delta_base_offset = cur.read_i32::<BigEndian>()?;
        let delta_base_offset = DeltaBaseOffset::new(delta_base_offset)?;
        let pack_entry_offset = cur.read_u64::<BigEndian>()?;
        let pack_entry_size = cur.read_u64::<BigEndian>()?;
        Ok(IndexEntry::new(
            node,
            delta_base_offset,
            pack_entry_offset,
            pack_entry_size,
        ))
    }

    fn write<T: Write>(&self, writer: &mut T) -> Fallible<()> {
        writer.write_all(self.node().as_ref())?;
        writer.write_i32::<BigEndian>(self.delta_base_offset().to_i32())?;
        writer.write_u64::<BigEndian>(self.pack_entry_offset())?;
        writer.write_u64::<BigEndian>(self.pack_entry_size())?;
        Ok(())
    }
}

impl DataIndexOptions {
    pub fn read<T: Read>(reader: &mut T) -> Fallible<DataIndexOptions> {
        let version = reader.read_u8()?;
        if version > 1 {
            return Err(DataIndexError(format!("unsupported version '{:?}'", version)).into());
        };

        let raw_config = reader.read_u8()?;
        let large = match raw_config {
            0b10000000 => true,
            0 => false,
            _ => {
                return Err(DataIndexError(format!("invalid data index '{:?}'", raw_config)).into());
            }
        };
        Ok(DataIndexOptions { version, large })
    }

    pub fn write<T: Write>(&self, writer: &mut T) -> Fallible<()> {
        writer.write_u8(self.version)?;
        writer.write_u8(if self.large { 0b10000000 } else { 0 })?;
        Ok(())
    }
}

pub struct DataIndex {
    mmap: Mmap,
    fanout_size: usize,
    index_start: usize,
}

impl DataIndex {
    pub fn new(path: &Path) -> Fallible<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len < 1 {
            return Err(DataIndexError(format!(
                "empty dataidx '{:?}' is invalid",
                path.to_str().unwrap_or("<unknown>")
            ))
            .into());
        }

        let mmap = unsafe { MmapOptions::new().len(len as usize).map(&file)? };
        let options = DataIndexOptions::read(&mut Cursor::new(&mmap))?;
        let fanout_size = FanoutTable::get_size(options.large);
        let mut index_start = 2 + fanout_size;

        // Version one records the number of entries in the index
        if options.version == 1 {
            index_start += 8;
        }

        Ok(DataIndex {
            mmap,
            fanout_size,
            index_start,
        })
    }

    pub fn write<T: Write>(writer: &mut T, values: &HashMap<Node, DeltaLocation>) -> Fallible<()> {
        // Write header
        let options = DataIndexOptions {
            version: 1,
            large: values.len() > SMALL_FANOUT_CUTOFF,
        };
        options.write(writer)?;

        let mut values: Vec<(&Node, &DeltaLocation)> = values.iter().collect();
        // They must be written in sorted order
        values.sort_by_key(|x| x.0);

        // Write fanout
        // `locations` will contain the eventual offset that each value will be written to.
        let mut locations: Vec<u32> = Vec::with_capacity(values.len());
        unsafe { locations.set_len(values.len()) };
        FanoutTable::write(
            writer,
            if options.large { 2 } else { 1 },
            &mut values.iter().map(|x| x.0),
            ENTRY_LEN,
            Some(&mut locations),
        )?;

        // Map from node to location
        let mut nodelocations: HashMap<Node, u32> = HashMap::new();
        for (i, &(node, _value)) in values.iter().enumerate() {
            nodelocations.insert(node.clone(), locations[i]);
        }

        // Write index
        writer.write_u64::<BigEndian>(values.len() as u64)?;
        for &(node, value) in values.iter() {
            let delta_base_offset =
                value
                    .delta_base
                    .map_or(DeltaBaseOffset::FullText, |delta_base| {
                        nodelocations
                            .get(&delta_base)
                            .map(|x| DeltaBaseOffset::Offset(*x as u32))
                            .unwrap_or(DeltaBaseOffset::Missing)
                    });

            let entry = IndexEntry::new(node.clone(), delta_base_offset, value.offset, value.size);

            entry.write(writer)?;
        }

        Ok(())
    }

    pub fn get_entry(&self, node: &Node) -> Fallible<IndexEntry> {
        let (start, end) = FanoutTable::get_bounds(self.get_fanout_slice(), node)?;
        let start = start + self.index_start;
        let end = match end {
            Option::None => self.mmap.len(),
            Option::Some(pos) => pos + self.index_start,
        };

        let entry_offset = self.binary_search(node, &self.mmap[start..end])?;
        self.read_entry((start + entry_offset) - self.index_start)
    }

    pub fn read_entry(&self, offset: usize) -> Fallible<IndexEntry> {
        let offset = offset + self.index_start;
        let raw_entry = self.mmap.get_err(offset..offset + ENTRY_LEN)?;
        IndexEntry::read(raw_entry)
    }

    fn binary_search(&self, key: &Node, slice: &[u8]) -> Fallible<usize> {
        let size = slice.len() / ENTRY_LEN;
        // Cast the slice into an array of entry buffers so we can bisect across them
        let slice: &[[u8; ENTRY_LEN]] =
            unsafe { ::std::slice::from_raw_parts(slice.as_ptr() as *const [u8; ENTRY_LEN], size) };
        match slice.binary_search_by(|entry| entry[0..20].cmp(key.as_ref())) {
            Ok(offset) => Ok(offset * ENTRY_LEN),
            Err(_offset) => Err(KeyError::new(
                DataIndexError(format!("no node {:?} in slice", key)).into(),
            )
            .into()),
        }
    }

    fn get_fanout_slice(&self) -> &[u8] {
        self.mmap[2..2 + self.fanout_size].as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use quickcheck::quickcheck;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use tempfile::NamedTempFile;

    fn make_index(values: &HashMap<Node, DeltaLocation>) -> DataIndex {
        let mut file = NamedTempFile::new().expect("file");
        DataIndex::write(&mut file, &values).expect("write dataindex");
        let path = file.into_temp_path();

        DataIndex::new(&path).expect("dataindex")
    }

    #[test]
    fn test_header_invalid() {
        let buf: Vec<u8> = vec![2, 0];
        DataIndexOptions::read(&mut Cursor::new(buf)).expect_err("invalid read");

        let buf: Vec<u8> = vec![0, 1];
        DataIndexOptions::read(&mut Cursor::new(buf)).expect_err("invalid read");
    }

    #[test]
    fn test_missing_delta_base() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let mut values: HashMap<Node, DeltaLocation> = HashMap::new();
        let node = Node::random(&mut rng);
        let base = Node::random(&mut rng);
        values.insert(
            node.clone(),
            DeltaLocation {
                delta_base: Some(base),
                offset: 1,
                size: 2,
            },
        );
        let index = make_index(&values);

        let delta = index.get_entry(&node).unwrap();
        assert_eq!(delta.delta_base_offset(), DeltaBaseOffset::Missing);
    }

    #[test]
    fn test_missing_key() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let mut values: HashMap<Node, DeltaLocation> = HashMap::new();
        let node = Node::random(&mut rng);
        values.insert(
            node.clone(),
            DeltaLocation {
                delta_base: None,
                offset: 1,
                size: 2,
            },
        );
        let index = make_index(&values);

        let other = Node::random(&mut rng);
        assert!(index
            .get_entry(&other)
            .unwrap_err()
            .downcast_ref::<KeyError>()
            .is_some())
    }

    quickcheck! {
        fn test_header_serialization(version: u8, large: bool) -> bool {
            let version = version % 2;
            let options = DataIndexOptions { version, large };
            let mut buf: Vec<u8> = vec![];
            options.write(&mut buf).expect("write");
            let parsed_options = DataIndexOptions::read(&mut Cursor::new(buf)).expect("read");
            options == parsed_options
        }

        fn test_roundtrip_index(nodes: Vec<(Node, u64)>) -> bool {
            let mut values: HashMap<Node, DeltaLocation> = HashMap::new();

            let mut nodes = nodes;
            let last_node = nodes.pop();

            let mut offset = 0;
            for &(node, size) in nodes.iter() {
                let size = size + 1;
                values.insert(
                    node.clone(),
                    DeltaLocation {
                        delta_base: Default::default(),
                        offset: offset,
                        size: size,
                    },
                );

                offset += size;
            }

            let index = make_index(&values);

            let mut offset = 0;
            for &(node, size) in nodes.iter() {
                let size = size + 1;
                let entry = index.get_entry(&node).expect("get_entry");
                assert_eq!(entry.node(), &node);
                assert_eq!(entry.delta_base_offset(), DeltaBaseOffset::FullText);
                assert_eq!(entry.pack_entry_offset(), offset);
                assert_eq!(entry.pack_entry_size(), size);
                offset += size;
            }

            let mut rng = ChaChaRng::from_seed([0u8; 32]);
            let last_node = last_node.unwrap_or((Node::random(&mut rng), 0)).0;
            index.get_entry(&last_node).expect_err("get_entry with missing node");

            true
        }
    }
}
