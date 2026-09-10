use binary_reader::BinaryReader;


#[derive(Clone)]
pub struct KeyTableEntry {
    pub section_id: u32, // TODO i32?
    pub cast_id: u32,    // TODO i32?
    pub fourcc: u32,
}

impl KeyTableEntry {
    pub fn from_reader(
        reader: &mut BinaryReader,
        _dir_version: u16,
    ) -> Result<KeyTableEntry, String> {
        return Ok(KeyTableEntry {
            section_id: reader.read_u32().unwrap(),
            cast_id: reader.read_u32().unwrap(),
            fourcc: reader.read_u32().unwrap(),
        });
    }
}

#[derive(Clone)]
pub struct KeyTableChunk {
    pub entry_size: u16, // Should always be 12 (3 uint32's)
    pub entry_size2: u16,
    pub entry_count: u32,
    pub used_count: u32,
    pub entries: Vec<KeyTableEntry>,
    /// Entry indices grouped by owning chunk (cast_id), so media lookups
    /// don't linearly rescan the whole table for every cast member.
    /// Large afterburned shared casts (the mizube project's system.cct)
    /// carry ~234k KEY* entries; a per-member scan is O(members x entries)
    /// and exhausts the WASM heap with temporary Vec allocations.
    pub by_cast_id: std::collections::HashMap<u32, Vec<usize>>,
}

impl KeyTableChunk {
    pub fn from_reader(
        reader: &mut BinaryReader,
        dir_version: u16,
    ) -> Result<KeyTableChunk, String> {
        let entry_size = reader.read_u16().unwrap();
        let entry_size2 = reader.read_u16().unwrap();
        let entry_count = reader.read_u32().unwrap();
        let used_count = reader.read_u32().unwrap();

        let entries: Vec<KeyTableEntry> = {
            let all_entries: Vec<KeyTableEntry> = (0..entry_count)
                .map(|_| KeyTableEntry::from_reader(reader, dir_version).unwrap())
                .collect();
            all_entries.into_iter().filter(|e| e.section_id > 0).collect()
        };

        let mut by_cast_id: std::collections::HashMap<u32, Vec<usize>> =
            std::collections::HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_cast_id.entry(e.cast_id).or_default().push(i);
        }

        return Ok(KeyTableChunk {
            entry_size: entry_size,
            entry_size2: entry_size2,
            entry_count: entry_count,
            used_count: used_count,
            entries,
            by_cast_id,
        });
    }
}
