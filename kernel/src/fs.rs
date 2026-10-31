//! `greyfs`: the filesystem this kernel can actually check.
//!
//! # The layout
//!
//! | sector  | contents                                                          |
//! | ------- | ----------------------------------------------------------------- |
//! | 0       | superblock                                                        |
//! | 1..=4   | the directory: 4 sectors x 16 entries x 32 bytes = 64 slots        |
//! | 5..     | file data, one contiguous run per file, allocated at `next_free`   |
//!
//! Everything is little-endian, and every multi-byte field is written and read by one
//! function per structure so the two cannot drift. The superblock ends in a seal (FNV-1a
//! over the bytes before it), because a magic number alone cannot tell a filesystem from a
//! disk whose first sector happens to start with the right eight bytes.
//!
//! # What this filesystem is not
//!
//! There are no directories: `apps/hello.gvm` is a name, not a path, and the slash is
//! allowed purely so names read naturally. There are no permissions, no timestamps, no
//! hard links and no journalling. A file is one extent, so replacing it leaks the old run
//! until a free-space pass exists, and removing it leaks its run entirely. Each of those
//! is a deliberate "not yet" rather than an oversight: what has to be true first is that
//! the format survives a round trip through a real disk, and that every one of these
//! refusals has a test.
//!
//! # What is where
//!
//! All of it is testable on the host: a [`Block`] is any type with sectors and two
//! transfers, so the tests mount an array. The kernel mounts the ATA driver from `disk`
//! through the same trait, which is what makes the tests evidence about the real path
//! rather than about a second implementation.

use alloc::string::String;
use alloc::vec::Vec;

use crate::disk::{sectors_for, Block, DiskError, SECTOR_BYTES};

/// The eight bytes that make a disk this filesystem's: `GREYFS` and two NULs.
pub const MAGIC: [u8; 8] = *b"GREYFS\0\0";

/// The only format this kernel writes or reads. A newer version is refused rather than
/// guessed at, because the layout below is the version.
pub const VERSION: u32 = 1;

/// Where the superblock lives.
pub const SUPERBLOCK_SECTOR: u64 = 0;
/// The first sector of the directory.
pub const DIR_FIRST_SECTOR: u64 = 1;
/// How many sectors the directory spans. Fixed, because a directory that could grow would
/// need an allocator for its own growth, which is the thing it does not have yet.
pub const DIR_SECTORS: u64 = 4;
/// One directory slot.
pub const ENTRY_BYTES: usize = 32;
/// Slots in one directory sector.
pub const ENTRIES_PER_SECTOR: usize = SECTOR_BYTES / ENTRY_BYTES;
/// Slots in the whole directory.
pub const MAX_ENTRIES: usize = DIR_SECTORS as usize * ENTRIES_PER_SECTOR;
/// The longest name, in bytes. Slashes are allowed and mean nothing; this is flat.
pub const NAME_MAX: usize = 22;
/// The first sector file data may occupy.
pub const DATA_FIRST_SECTOR: u64 = DIR_FIRST_SECTOR + DIR_SECTORS;
/// The smallest device this filesystem fits on: a superblock, a directory, no data.
pub const MIN_SECTORS: u64 = DATA_FIRST_SECTOR;

/// What an entry describes. Kept on disk so a program is distinguishable from text
/// without parsing it, which M9's loader wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Bytes to read.
    File,
    /// Bytes to read and execute: a `.gvm` program, from M9 on.
    Program,
}

impl Kind {
    /// The byte the format stores.
    pub const fn code(self) -> u8 {
        match self {
            Kind::File => 0,
            Kind::Program => 1,
        }
    }

    /// The kind a stored byte names, or `None` if it is not one this format has.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Kind::File),
            1 => Some(Kind::Program),
            _ => None,
        }
    }
}

/// Everything that can go wrong between a name and its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// The device refused a transfer; carries the driver's own reason.
    Disk(DiskError),
    /// There is no room left: no sectors, or no directory slots.
    NoSpace(&'static str),
    /// The name is not in the directory.
    NotFound,
    /// A name longer than [`NAME_MAX`].
    NameTooLong,
    /// An empty name, a leading or trailing slash, a doubled slash, or a byte outside the
    /// name alphabet.
    NameInvalid,
    /// The first sector is not this filesystem's.
    BadMagic,
    /// A version this kernel does not read.
    BadVersion(u32),
    /// A sector size this kernel does not read.
    BadSectorSize(u32),
    /// The seal does not match: the superblock is damaged, not merely absent.
    BadSeal,
    /// A field the format says cannot be what the disk says it is.
    Corrupt(&'static str),
}

impl From<DiskError> for FsError {
    fn from(error: DiskError) -> Self {
        FsError::Disk(error)
    }
}

/// FNV-1a, the smallest checksum that is not a joke.
///
/// It is not a cryptographic hash and does not need to be: it has to tell a superblock
/// this kernel wrote from one a truncated write or a stray sector left behind. A CRC would
/// be stronger and is not worth the table.
pub const fn seal(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        index += 1;
    }
    hash
}

/// Whether `name` is one this filesystem will store.
///
/// The alphabet is deliberately small: letters, digits, `.`, `_`, `-` and `/`. A name is
/// printed in the boot log and typed at a shell prompt, so a byte that needs escaping to
/// be read is a name that should not be allowed to exist. The slash is included so names
/// can read like paths, and it may not lead, trail or double.
pub fn check_name(name: &str) -> Result<(), FsError> {
    if name.is_empty() {
        return Err(FsError::NameInvalid);
    }
    if name.len() > NAME_MAX {
        return Err(FsError::NameTooLong);
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(FsError::NameInvalid);
    }
    for byte in name.bytes() {
        let allowed = byte.is_ascii_alphanumeric()
            || byte == b'.'
            || byte == b'_'
            || byte == b'-'
            || byte == b'/';
        if !allowed {
            return Err(FsError::NameInvalid);
        }
    }
    Ok(())
}

/// The fields of sector zero, after the magic and before the seal.
///
/// `next_free` is the whole allocator: everything below it is metadata or a live extent,
/// everything at or above it has never been written. `generation` counts mounts, which is
/// how a boot proves it read a filesystem an earlier boot wrote rather than one it just
/// made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Superblock {
    /// The format version, always [`VERSION`] for a filesystem this kernel accepts.
    pub version: u32,
    /// Always [`SECTOR_BYTES`], stored so a mismatch is a refusal and not a guess.
    pub sector_bytes: u32,
    /// The device's size when it was formatted.
    pub sectors: u64,
    /// The first sector that has never been written.
    pub next_free: u64,
    /// How many times this filesystem has been mounted.
    pub generation: u32,
}

impl Superblock {
    /// The bytes of sector zero for this superblock.
    ///
    /// Written as one function so the seal can never be computed over a different byte
    /// range than the one the disk holds.
    pub fn encode(self) -> [u8; SECTOR_BYTES] {
        let mut sector = [0u8; SECTOR_BYTES];
        sector[0..8].copy_from_slice(&MAGIC);
        sector[8..12].copy_from_slice(&self.version.to_le_bytes());
        sector[12..16].copy_from_slice(&self.sector_bytes.to_le_bytes());
        sector[16..24].copy_from_slice(&self.sectors.to_le_bytes());
        sector[24..32].copy_from_slice(&self.next_free.to_le_bytes());
        sector[32..36].copy_from_slice(&self.generation.to_le_bytes());
        let checksum = seal(&sector[..36]);
        sector[36..40].copy_from_slice(&checksum.to_le_bytes());
        sector
    }

    /// The superblock a sector zero describes.
    ///
    /// The magic is checked before anything is read out of the rest, and the seal after
    /// the fields that can be read without trusting them, so a sector of zeros and a
    /// damaged superblock produce different errors - which is what lets [`Filesystem::open`]
    /// format only the former.
    pub fn decode(sector: &[u8; SECTOR_BYTES]) -> Result<Self, FsError> {
        if sector[0..8] != MAGIC {
            return Err(FsError::BadMagic);
        }
        let stored = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
        if stored != seal(&sector[..36]) {
            return Err(FsError::BadSeal);
        }
        let superblock = Superblock {
            version: u32::from_le_bytes([sector[8], sector[9], sector[10], sector[11]]),
            sector_bytes: u32::from_le_bytes([sector[12], sector[13], sector[14], sector[15]]),
            sectors: u64::from_le_bytes(sector[16..24].try_into().expect("eight bytes")),
            next_free: u64::from_le_bytes(sector[24..32].try_into().expect("eight bytes")),
            generation: u32::from_le_bytes([sector[32], sector[33], sector[34], sector[35]]),
        };
        if superblock.version != VERSION {
            return Err(FsError::BadVersion(superblock.version));
        }
        if superblock.sector_bytes != SECTOR_BYTES as u32 {
            return Err(FsError::BadSectorSize(superblock.sector_bytes));
        }
        if superblock.next_free < MIN_SECTORS || superblock.next_free > superblock.sectors {
            return Err(FsError::Corrupt("next_free outside the device"));
        }
        Ok(superblock)
    }
}

/// One directory slot's worth of information.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    /// The stored name, without padding.
    pub name: String,
    /// Whether these bytes are a file or a program.
    pub kind: Kind,
    /// The first sector of the extent.
    pub start: u64,
    /// The extent's length in bytes, not counting the padding of its last sector.
    pub len: u64,
}

impl Entry {
    /// The bytes of one directory slot.
    pub fn encode(&self) -> [u8; ENTRY_BYTES] {
        let mut slot = [0u8; ENTRY_BYTES];
        let name = self.name.as_bytes();
        slot[..name.len()].copy_from_slice(name);
        slot[22] = self.kind.code();
        slot[24..28].copy_from_slice(&(self.start as u32).to_le_bytes());
        slot[28..32].copy_from_slice(&(self.len as u32).to_le_bytes());
        slot
    }

    /// The entry one slot describes, or `None` if the slot is empty.
    ///
    /// A slot is empty when its name starts with a NUL. Anything else that does not decode
    /// is [`FsError::Corrupt`]: a directory that silently skipped a slot it could not read
    /// would hide the file that slot names.
    pub fn decode(slot: &[u8]) -> Result<Option<Self>, FsError> {
        if slot[0] == 0 {
            return Ok(None);
        }
        let name_end = slot[..22].iter().position(|byte| *byte == 0).unwrap_or(22);
        let name = core::str::from_utf8(&slot[..name_end])
            .map_err(|_| FsError::Corrupt("a directory name is not utf-8"))?;
        let name = String::from(name);
        check_name(&name)?;
        let kind = Kind::from_code(slot[22]).ok_or(FsError::Corrupt("unknown entry kind"))?;
        let start = u32::from_le_bytes([slot[24], slot[25], slot[26], slot[27]]) as u64;
        let len = u32::from_le_bytes([slot[28], slot[29], slot[30], slot[31]]) as u64;
        Ok(Some(Entry {
            name,
            kind,
            start,
            len,
        }))
    }

    /// The sectors the extent occupies, rounded up: what a read has to ask the disk for.
    pub fn sectors(&self) -> u64 {
        sectors_for(self.len as usize)
    }
}

/// What [`Filesystem::open`] found on the device.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Opened {
    /// There was no filesystem, so one was written.
    Formatted,
    /// There was one, and it was mounted.
    Mounted,
}

/// A mounted `greyfs`.
///
/// The superblock and the directory are cached in memory and written back on every change:
/// a directory sector is one 512-byte transfer, and a filesystem this small has no need to
/// keep a dirty list. Nothing is buffered lazily, so an unmount is just dropping the value.
pub struct Filesystem<D: Block> {
    disk: D,
    superblock: Superblock,
    /// The directory, held positionally: slot `index` here is slot `index` on the disk.
    /// Keeping the two in step by position rather than by packing live entries to the
    /// front is what makes a removal a single zeroed slot instead of a rewrite of the
    /// whole directory.
    slots: Vec<Option<Entry>>,
}

impl<D: Block> Filesystem<D> {
    /// Writes an empty filesystem over `disk`, destroying whatever was there.
    ///
    /// Only ever called for a device with nothing on it - [`Self::open`] is what decides
    /// that, and refuses to format anything that already has a superblock.
    pub fn format(mut disk: D) -> Result<Self, FsError> {
        if disk.sectors() < MIN_SECTORS {
            return Err(FsError::NoSpace(
                "the device is too small for a superblock and a directory",
            ));
        }
        let blank = [0u8; SECTOR_BYTES];
        for sector in 0..DIR_SECTORS {
            disk.write(DIR_FIRST_SECTOR + sector, &blank)?;
        }
        let superblock = Superblock {
            version: VERSION,
            sector_bytes: SECTOR_BYTES as u32,
            sectors: disk.sectors(),
            next_free: DATA_FIRST_SECTOR,
            generation: 0,
        };
        disk.write(SUPERBLOCK_SECTOR, &superblock.encode())?;
        Ok(Self {
            disk,
            superblock,
            slots: alloc::vec![None; MAX_ENTRIES],
        })
    }

    /// Mounts the filesystem already on `disk`, bumping its generation.
    pub fn mount(mut disk: D) -> Result<Self, FsError> {
        let mut sector = [0u8; SECTOR_BYTES];
        disk.read(SUPERBLOCK_SECTOR, &mut sector)?;
        Self::mount_with(disk, sector)
    }

    /// Mounts the filesystem on `disk`, or formats it when the first sector is all zeros.
    ///
    /// The distinction matters: a blank disk is a disk nobody has used, and a damaged
    /// superblock is somebody's data. Formatting the first is what makes a fresh image
    /// bootable; formatting the second would destroy the evidence of whatever wrote it.
    pub fn open(mut disk: D) -> Result<(Self, Opened), FsError> {
        let mut sector = [0u8; SECTOR_BYTES];
        disk.read(SUPERBLOCK_SECTOR, &mut sector)?;
        if sector.iter().all(|byte| *byte == 0) {
            return Self::format(disk).map(|fs| (fs, Opened::Formatted));
        }
        Self::mount_with(disk, sector).map(|fs| (fs, Opened::Mounted))
    }

    /// Mounts from a superblock sector that is already in hand.
    fn mount_with(mut disk: D, sector: [u8; SECTOR_BYTES]) -> Result<Self, FsError> {
        let mut superblock = Superblock::decode(&sector)?;
        superblock.generation = superblock.generation.wrapping_add(1);
        disk.write(SUPERBLOCK_SECTOR, &superblock.encode())?;
        let slots = Self::load_directory(&mut disk, &superblock)?;
        Ok(Self {
            disk,
            superblock,
            slots,
        })
    }

    /// Reads the directory and refuses anything that contradicts the superblock.
    ///
    /// Every check here is one a corrupt sector would trip: an extent that starts inside
    /// the metadata, an extent that runs into the unallocated region, a length that cannot
    /// be represented, or two entries with one name. A mount that carried on past any of
    /// them would hand out a file whose bytes belong to another.
    fn load_directory(
        disk: &mut D,
        superblock: &Superblock,
    ) -> Result<Vec<Option<Entry>>, FsError> {
        let mut buffer = alloc::vec![0u8; DIR_SECTORS as usize * SECTOR_BYTES];
        disk.read(DIR_FIRST_SECTOR, &mut buffer)?;
        let mut slots: Vec<Option<Entry>> = alloc::vec![None; MAX_ENTRIES];
        for (index, slot) in buffer.as_chunks::<ENTRY_BYTES>().0.iter().enumerate() {
            let Some(entry) = Entry::decode(slot)? else {
                continue;
            };
            if entry.start < DATA_FIRST_SECTOR {
                return Err(FsError::Corrupt("an extent starts inside the metadata"));
            }
            let end = match entry.start.checked_add(entry.sectors()) {
                Some(end) => end,
                None => return Err(FsError::Corrupt("an extent wraps the device")),
            };
            if end > superblock.next_free || end > superblock.sectors {
                return Err(FsError::Corrupt("an extent runs into unallocated sectors"));
            }
            if entry.len > u32::MAX as u64 {
                return Err(FsError::Corrupt(
                    "an extent is longer than the format allows",
                ));
            }
            if slots.iter().flatten().any(|other| other.name == entry.name) {
                return Err(FsError::Corrupt("two entries share a name"));
            }
            slots[index] = Some(entry);
        }
        Ok(slots)
    }
}

impl<D: Block> Filesystem<D> {
    /// The live entries, in slot order.
    pub fn list(&self) -> impl Iterator<Item = &Entry> {
        self.slots.iter().flatten()
    }

    /// How many files the directory holds.
    pub fn count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// The slot a name lives in, or `None`.
    pub fn find(&self, name: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|entry| entry.name == name))
    }

    /// The entry a name names, without reading its bytes.
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.find(name).and_then(|index| self.slots[index].as_ref())
    }

    /// The superblock as the mount found it, for the boot log.
    pub fn superblock(&self) -> Superblock {
        self.superblock
    }

    /// The first sector that has never been written: the whole allocator's state.
    pub fn next_free(&self) -> u64 {
        self.superblock.next_free
    }

    /// The sectors a new extent could still use.
    pub fn free_sectors(&self) -> u64 {
        self.superblock
            .sectors
            .saturating_sub(self.superblock.next_free)
    }

    /// Reads a file's bytes.
    ///
    /// The extent's last sector is read whole and then cut to the length the entry
    /// records, which is the only place the padding is visible.
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>, FsError> {
        let entry = self.entry(name).cloned().ok_or(FsError::NotFound)?;
        let mut buffer = alloc::vec![0u8; entry.sectors() as usize * SECTOR_BYTES];
        if !buffer.is_empty() {
            self.disk.read(entry.start, &mut buffer)?;
        }
        buffer.truncate(entry.len as usize);
        Ok(buffer)
    }

    /// Writes `bytes` to `name`, creating it or replacing it, and returns the entry.
    ///
    /// The order is data, then directory, then superblock. A crash between the first two
    /// leaves a file whose extent `next_free` will hand out again - a lost write. A crash
    /// between the last two leaves the directory claiming an extent the superblock says is
    /// unallocated, which the next mount refuses. Both are better than the third order,
    /// where a crash leaves a file pointing at sectors that another file has claimed.
    ///
    /// Replacing a file leaks its old extent. That is the honest cost of having no free
    /// list: the milestone that adds one has to add the invariant with it.
    pub fn write(&mut self, name: &str, bytes: &[u8], kind: Kind) -> Result<Entry, FsError> {
        check_name(name)?;
        let sectors = sectors_for(bytes.len());
        let start = self.superblock.next_free;
        let end = match start.checked_add(sectors) {
            Some(end) if end <= self.superblock.sectors => end,
            _ => return Err(FsError::NoSpace("the device has no room for this file")),
        };
        if end > u32::MAX as u64 {
            return Err(FsError::NoSpace("the extent does not fit in 32 bits"));
        }
        let slot = match self.find(name) {
            Some(index) => index,
            None => match self.slots.iter().position(Option::is_none) {
                Some(index) => index,
                None => return Err(FsError::NoSpace("the directory has no free slot")),
            },
        };
        if sectors > 0 {
            let mut padded = alloc::vec![0u8; sectors as usize * SECTOR_BYTES];
            padded[..bytes.len()].copy_from_slice(bytes);
            self.disk.write(start, &padded)?;
        }
        let entry = Entry {
            name: String::from(name),
            kind,
            start,
            len: bytes.len() as u64,
        };
        self.store_entry(slot, &entry)?;
        self.superblock.next_free = end;
        self.store_superblock()?;
        Ok(entry)
    }

    /// Removes a name and returns what it described.
    ///
    /// The extent is not reclaimed: without a free list, the only safe statement about
    /// those sectors is that nothing points at them any more.
    pub fn remove(&mut self, name: &str) -> Result<Entry, FsError> {
        let slot = self.find(name).ok_or(FsError::NotFound)?;
        let entry = self.slots[slot].clone().ok_or(FsError::NotFound)?;
        let sector = DIR_FIRST_SECTOR + (slot / ENTRIES_PER_SECTOR) as u64;
        let offset = (slot % ENTRIES_PER_SECTOR) * ENTRY_BYTES;
        let mut buffer = [0u8; SECTOR_BYTES];
        self.disk.read(sector, &mut buffer)?;
        buffer[offset..offset + ENTRY_BYTES].fill(0);
        self.disk.write(sector, &buffer)?;
        self.slots[slot] = None;
        Ok(entry)
    }

    /// Hands the device back, which is how a test remounts the same bytes.
    pub fn into_disk(self) -> D {
        self.disk
    }

    /// Writes one slot and keeps the cache in step, so a failed write cannot leave the
    /// two disagreeing about which slot holds what.
    fn store_entry(&mut self, slot: usize, entry: &Entry) -> Result<(), FsError> {
        let sector = DIR_FIRST_SECTOR + (slot / ENTRIES_PER_SECTOR) as u64;
        let offset = (slot % ENTRIES_PER_SECTOR) * ENTRY_BYTES;
        let mut buffer = [0u8; SECTOR_BYTES];
        self.disk.read(sector, &mut buffer)?;
        buffer[offset..offset + ENTRY_BYTES].copy_from_slice(&entry.encode());
        self.disk.write(sector, &buffer)?;
        self.slots[slot] = Some(entry.clone());
        Ok(())
    }

    /// Rewrites sector zero. The seal is recomputed every time by [`Superblock::encode`].
    fn store_superblock(&mut self) -> Result<(), FsError> {
        self.disk
            .write(SUPERBLOCK_SECTOR, &self.superblock.encode())
            .map_err(FsError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::transfer_fits;

    /// A device that is an array: the whole filesystem, on the host, with no QEMU.
    struct MemDisk {
        sectors: u64,
        bytes: Vec<u8>,
    }

    impl MemDisk {
        fn new(sectors: u64) -> Self {
            Self {
                sectors,
                bytes: vec![0u8; sectors as usize * SECTOR_BYTES],
            }
        }

        fn sector_mut(&mut self, lba: u64) -> &mut [u8] {
            let start = lba as usize * SECTOR_BYTES;
            &mut self.bytes[start..start + SECTOR_BYTES]
        }
    }

    impl Block for MemDisk {
        fn sectors(&self) -> u64 {
            self.sectors
        }

        fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), DiskError> {
            if !transfer_fits(lba, buffer.len(), self.sectors) {
                return Err(DiskError::OutOfRange {
                    lba,
                    sectors: self.sectors,
                });
            }
            let start = lba as usize * SECTOR_BYTES;
            buffer.copy_from_slice(&self.bytes[start..start + buffer.len()]);
            Ok(())
        }

        fn write(&mut self, lba: u64, source: &[u8]) -> Result<(), DiskError> {
            if !transfer_fits(lba, source.len(), self.sectors) {
                return Err(DiskError::OutOfRange {
                    lba,
                    sectors: self.sectors,
                });
            }
            let start = lba as usize * SECTOR_BYTES;
            self.bytes[start..start + source.len()].copy_from_slice(source);
            Ok(())
        }
    }

    fn formatted(sectors: u64) -> Filesystem<MemDisk> {
        Filesystem::format(MemDisk::new(sectors)).expect("a fresh device can be formatted")
    }

    #[test]
    fn a_fresh_filesystem_has_no_files_and_a_known_first_sector() {
        let fs = formatted(512);
        assert_eq!(fs.count(), 0);
        assert_eq!(fs.superblock().next_free, DATA_FIRST_SECTOR);
        assert_eq!(fs.superblock().version, VERSION);
        assert_eq!(fs.free_sectors(), 512 - DATA_FIRST_SECTOR);
    }

    #[test]
    fn a_mount_after_a_remount_sees_the_same_files() {
        let mut fs = formatted(512);
        fs.write("motd", b"hello from a disk", Kind::File)
            .expect("write");
        let remounted = Filesystem::mount(fs.into_disk()).expect("mount");
        assert_eq!(remounted.count(), 1);
        let entry = remounted.entry("motd").expect("the entry survived");
        assert_eq!(entry.len, 17);
        assert_eq!(entry.kind, Kind::File);
        // One generation per mount: the second mount is generation one.
        assert_eq!(remounted.superblock().generation, 1);
    }

    #[test]
    fn a_file_written_is_a_file_read() {
        let mut fs = formatted(512);
        let payload: Vec<u8> = (0..1500u32).map(|value| value as u8).collect();
        fs.write("pattern.bin", &payload, Kind::File)
            .expect("write");
        assert_eq!(fs.read("pattern.bin").expect("read"), payload);
        // 1500 bytes is three sectors, and the entry records the length, not the extent.
        assert_eq!(fs.entry("pattern.bin").expect("entry").len, 1500);
        assert_eq!(fs.entry("pattern.bin").expect("entry").sectors(), 3);
    }

    #[test]
    fn an_empty_file_reads_as_empty_and_costs_no_sectors() {
        let mut fs = formatted(512);
        let before = fs.next_free();
        fs.write("empty", b"", Kind::File).expect("write");
        assert_eq!(fs.next_free(), before);
        assert_eq!(fs.read("empty").expect("read"), Vec::<u8>::new());
        assert_eq!(fs.count(), 1);
    }

    #[test]
    fn replacing_a_file_moves_it_to_a_fresh_extent() {
        let mut fs = formatted(512);
        let first = fs.write("log", b"first", Kind::File).expect("write");
        let second = fs.write("log", b"second", Kind::File).expect("rewrite");
        assert_ne!(first.start, second.start);
        assert_eq!(fs.count(), 1);
        assert_eq!(fs.read("log").expect("read"), b"second".to_vec());
    }

    #[test]
    fn removing_a_file_keeps_the_others_and_the_remount_agrees() {
        let mut fs = formatted(512);
        fs.write("a", b"one", Kind::File).expect("write a");
        fs.write("b", b"two", Kind::File).expect("write b");
        let removed = fs.remove("a").expect("remove");
        assert_eq!(removed.name, "a");
        assert_eq!(fs.count(), 1);
        assert_eq!(fs.read("a"), Err(FsError::NotFound));
        assert_eq!(fs.read("b").expect("read b"), b"two".to_vec());
        let mut fs = Filesystem::mount(fs.into_disk()).expect("mount");
        assert_eq!(fs.count(), 1);
        assert_eq!(fs.read("b").expect("read b"), b"two".to_vec());
    }

    #[test]
    fn a_file_that_does_not_fit_is_refused_and_leaves_the_count_alone() {
        // One sector of data after the metadata.
        let mut fs = formatted(MIN_SECTORS + 1);
        let too_big = vec![0u8; 2 * SECTOR_BYTES];
        assert_eq!(
            fs.write("big", &too_big, Kind::File),
            Err(FsError::NoSpace("the device has no room for this file"))
        );
        assert_eq!(fs.count(), 0);
        assert_eq!(fs.next_free(), DATA_FIRST_SECTOR);
        // Exactly one sector still fits.
        fs.write("fits", &vec![7u8; SECTOR_BYTES], Kind::File)
            .expect("one sector fits");
        assert_eq!(fs.free_sectors(), 0);
    }

    #[test]
    fn the_directory_runs_out_of_slots_before_the_disk_runs_out_of_room() {
        let mut fs = formatted(512);
        for index in 0..MAX_ENTRIES {
            let name = format!("f{index:02}");
            fs.write(&name, b"x", Kind::File).expect("a slot was free");
        }
        assert_eq!(fs.count(), MAX_ENTRIES);
        assert_eq!(
            fs.write("one-too-many", b"x", Kind::File),
            Err(FsError::NoSpace("the directory has no free slot"))
        );
        // A name that already exists still fits: a replacement reuses its slot.
        fs.write("f00", b"y", Kind::File).expect("replace");
        assert_eq!(fs.count(), MAX_ENTRIES);
    }

    #[test]
    fn names_outside_the_alphabet_are_refused() {
        for name in ["", "/a", "a/", "a//b", "a b", "a\tb", "a:b", "a*"] {
            assert_eq!(check_name(name), Err(FsError::NameInvalid), "{name:?}");
        }
        let long = "a".repeat(NAME_MAX + 1);
        assert_eq!(check_name(&long), Err(FsError::NameTooLong));
        assert!(check_name(&"a".repeat(NAME_MAX)).is_ok());
        for name in ["motd", "apps/hello.gvm", "a-b_c.1"] {
            assert!(check_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn a_damaged_superblock_is_refused_rather_than_mounted() {
        let fs = formatted(512);
        let mut disk = fs.into_disk();
        // One byte inside the size field, which the seal covers.
        disk.sector_mut(SUPERBLOCK_SECTOR)[20] ^= 0x01;
        assert_eq!(Filesystem::mount(disk).err(), Some(FsError::BadSeal));
    }

    #[test]
    fn a_blank_device_is_formatted_and_a_used_one_is_mounted() {
        let disk = MemDisk::new(512);
        let (fs, opened) = Filesystem::open(disk).expect("open formats a blank device");
        assert_eq!(opened, Opened::Formatted);
        let (fs, opened) = Filesystem::open(fs.into_disk()).expect("open mounts a used one");
        assert_eq!(opened, Opened::Mounted);
        assert_eq!(fs.superblock().generation, 1);
    }

    #[test]
    fn a_device_with_someone_elses_superblock_is_not_formatted_over() {
        let mut disk = MemDisk::new(512);
        disk.sector_mut(SUPERBLOCK_SECTOR)[0] = 0x42;
        assert_eq!(Filesystem::open(disk).err(), Some(FsError::BadMagic));
    }

    #[test]
    fn a_version_or_sector_size_this_kernel_does_not_read_is_refused() {
        let good = Superblock {
            version: VERSION,
            sector_bytes: SECTOR_BYTES as u32,
            sectors: 512,
            next_free: DATA_FIRST_SECTOR,
            generation: 0,
        };
        assert_eq!(Superblock::decode(&good.encode()), Ok(good));

        let mut sector = good.encode();
        sector[8..12].copy_from_slice(&2u32.to_le_bytes());
        let checksum = super::seal(&sector[..36]);
        sector[36..40].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(Superblock::decode(&sector), Err(FsError::BadVersion(2)));

        let mut wrong_size = good;
        wrong_size.sector_bytes = 4096;
        assert_eq!(
            Superblock::decode(&wrong_size.encode()),
            Err(FsError::BadSectorSize(4096))
        );

        // A next_free below the data, or past the device, is corruption rather than a
        // version problem.
        let mut below = good;
        below.next_free = DIR_FIRST_SECTOR;
        assert_eq!(
            Superblock::decode(&below.encode()),
            Err(FsError::Corrupt("next_free outside the device"))
        );
        let mut past = good;
        past.next_free = 513;
        assert_eq!(
            Superblock::decode(&past.encode()),
            Err(FsError::Corrupt("next_free outside the device"))
        );
    }

    #[test]
    fn an_extent_outside_the_allocated_region_is_refused() {
        let mut disk = MemDisk::new(512);
        let superblock = Superblock {
            version: VERSION,
            sector_bytes: SECTOR_BYTES as u32,
            sectors: 512,
            next_free: DATA_FIRST_SECTOR,
            generation: 0,
        };
        disk.sector_mut(SUPERBLOCK_SECTOR)
            .copy_from_slice(&superblock.encode());
        let entry = Entry {
            name: String::from("ghost"),
            kind: Kind::File,
            start: DATA_FIRST_SECTOR,
            len: 4096,
        };
        disk.sector_mut(DIR_FIRST_SECTOR)[..ENTRY_BYTES].copy_from_slice(&entry.encode());
        assert_eq!(
            Filesystem::mount(disk).err(),
            Some(FsError::Corrupt("an extent runs into unallocated sectors"))
        );
    }

    #[test]
    fn two_entries_with_one_name_are_refused() {
        let mut disk = MemDisk::new(512);
        let superblock = Superblock {
            version: VERSION,
            sector_bytes: SECTOR_BYTES as u32,
            sectors: 512,
            next_free: DATA_FIRST_SECTOR + 1,
            generation: 0,
        };
        disk.sector_mut(SUPERBLOCK_SECTOR)
            .copy_from_slice(&superblock.encode());
        let entry = Entry {
            name: String::from("twin"),
            kind: Kind::File,
            start: DATA_FIRST_SECTOR,
            len: 4,
        };
        let sector = disk.sector_mut(DIR_FIRST_SECTOR);
        sector[..ENTRY_BYTES].copy_from_slice(&entry.encode());
        sector[ENTRY_BYTES..ENTRY_BYTES * 2].copy_from_slice(&entry.encode());
        assert_eq!(
            Filesystem::mount(disk).err(),
            Some(FsError::Corrupt("two entries share a name"))
        );
    }

    #[test]
    fn the_kind_of_a_file_survives_a_remount() {
        let mut fs = formatted(512);
        fs.write("apps/hello.gvm", b"GVM1", Kind::Program)
            .expect("write");
        let fs = Filesystem::mount(fs.into_disk()).expect("mount");
        assert_eq!(
            fs.entry("apps/hello.gvm").expect("entry").kind,
            Kind::Program
        );
    }

    #[test]
    fn the_seal_covers_every_field_before_it() {
        let superblock = Superblock {
            version: VERSION,
            sector_bytes: SECTOR_BYTES as u32,
            sectors: 512,
            next_free: DATA_FIRST_SECTOR,
            generation: 7,
        };
        let encoded = superblock.encode();
        // Every byte the format defines is inside the seal's range.
        for offset in 0..36 {
            let mut damaged = encoded;
            damaged[offset] ^= 0xff;
            assert!(
                Superblock::decode(&damaged).is_err(),
                "byte {offset} is not covered by the seal"
            );
        }
    }
}
