//! The first storage driver: an ATA controller in PIO mode.
//!
//! # Why PIO, and why the primary channel
//!
//! The point of the driver is that the filesystem above it has to talk to something with
//! sectors, a status register and failures. The parallel ATA interface in programmed-I/O
//! mode is the smallest thing that is still a real disk: two ports for the data and the
//! command block, a 28-bit block address, and a busy/done handshake. It needs no PCI
//! enumeration, no interrupt, no DMA descriptor and no bus-master programming, and QEMU's
//! `-drive if=ide` puts a disk behind exactly those ports. A virtio or AHCI driver is the
//! right thing once there is a PCI bus to walk; this is the right thing to prove a
//! filesystem with.
//!
//! # What is testable where
//!
//! [`Block`] is the interface the filesystem mounts: sectors and two transfers, so `fs`
//! can be tested on the host against an array. The half of this module that touches ports
//! is compiled out there, because `in`/`out` do not exist on the machine running the tests -
//! the same split `paging` makes between its arithmetic and its `invlpg`. What stays in the
//! open is the part with the interesting arithmetic: the drive/head byte that carries bits
//! 24-27 of the block address, the capability word that says whether a drive can use that
//! address at all, the size in words 60 and 61, and the model string, which is stored as
//! byte-swapped words.

use alloc::string::String;

/// Bytes in an ATA sector. The filesystem's block size, and the unit every transfer here
/// is counted in.
pub const SECTOR_BYTES: usize = 512;

/// The largest transfer the driver issues in one command: 128 sectors, 64 KiB. The count
/// register holds 0..=255 with 0 meaning 256, and keeping the driver's own chunks well
/// under that keeps each command's timeout short enough to mean something.
pub const CHUNK_SECTORS: usize = 128;

/// What a block device has to provide: the filesystem's whole world.
///
/// Taking the device by value in `fs::Filesystem` rather than by `&mut` reference is what
/// keeps the filesystem free of lifetimes; in the kernel the device is the one controller
/// this machine has.
pub trait Block {
    /// How many [`SECTOR_BYTES`]-sized sectors the device holds.
    fn sectors(&self) -> u64;

    /// Reads whole sectors into `buffer`, which must be a multiple of [`SECTOR_BYTES`].
    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), DiskError>;

    /// Writes whole sectors from `source`, which must be a multiple of [`SECTOR_BYTES`].
    fn write(&mut self, lba: u64, source: &[u8]) -> Result<(), DiskError>;
}

/// Everything that can go wrong between a request and the platter.
///
/// The variants are separate rather than one `Failed` because the report the kernel prints
/// is the only diagnosis a machine with no serial cable will ever give: "the drive was not
/// there" and "the drive raised an error" have nothing to do with each other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskError {
    /// Nothing answered on the channel, or the floating-bus check said no drive.
    NoDevice,
    /// The drive is there but does not implement 28-bit block addressing.
    NotLba28,
    /// A transfer whose length is not a whole number of sectors.
    NotSectorAligned,
    /// A transfer past the end of the device, named so the caller can see which.
    OutOfRange { lba: u64, sectors: u64 },
    /// The controller never cleared busy.
    Timeout,
    /// The controller set `ERR` or `DF`: the drive refused the command.
    DeviceFault,
    /// The controller cleared busy without ever asking for a data transfer.
    NoData,
}

/// Bits of the status register, which is also the command register's read half.
pub mod status {
    /// The last command failed; read the error register for why.
    pub const ERR: u8 = 1 << 0;
    /// Data request: set when a sector is ready to be read or written.
    pub const DRQ: u8 = 1 << 3;
    /// Device fault.
    pub const DF: u8 = 1 << 5;
    /// The controller is busy and will not accept a command.
    pub const BSY: u8 = 1 << 7;
}

/// The drive/head byte for a 28-bit block address.
///
/// Bits 0-3 carry the address's top nibble, bit 4 selects the drive, and bits 5-7 are
/// historical (`1`, `1`, `0`) - the ATA name for them is "obsolete, set to one". Getting
/// this byte wrong on a disk larger than 128 GiB silently addresses the wrong region, which
/// is why it is a function with a test rather than an expression inside the command
/// sequence.
pub const fn drive_head(lba: u64, master: bool) -> u8 {
    let drive = if master { 0 } else { 1 };
    0xe0 | (drive << 4) | ((lba >> 24) & 0x0f) as u8
}

/// Whether identify's word 49 says the drive implements 28-bit LBA.
///
/// Bit 9 is the capability; a drive that only offers CHS is not one this driver can use,
/// and saying so once at probe time beats every transfer failing later.
pub const fn supports_lba28(identify_word_49: u16) -> bool {
    identify_word_49 & (1 << 9) != 0
}

/// The drive's size in sectors, from identify words 60 and 61.
///
/// Word 60 is the low half and word 61 the high half, both little-endian, which makes the
/// pair one 32-bit sector count.
pub const fn lba28_sectors(word_60: u16, word_61: u16) -> u64 {
    (word_60 as u64) | ((word_61 as u64) << 16)
}

/// Reads the model string out of an identify buffer.
///
/// Words 27..=46 hold 40 characters as big-endian pairs, space padded. The reversal is
/// the classic ATA trap: a little-endian read of that range produces `"MEUQ HARDDISK"`,
/// which looks right in a hex dump and wrong in a log line.
pub fn model(identify: &[u16; 256]) -> String {
    let mut text = String::with_capacity(40);
    for word in &identify[27..47] {
        for byte in word.to_be_bytes() {
            if byte != 0 {
                text.push(char::from(byte));
            }
        }
    }
    let trimmed = text.trim_end();
    let mut result = String::with_capacity(trimmed.len());
    result.push_str(trimmed);
    result
}

/// A sector count of zero means 256: the register cannot express it, so ATA redefines
/// zero. Kept as a function because the driver's chunking has to agree with it.
pub const fn count_register(sectors: usize) -> u8 {
    // 256 & 0xff is 0, which is exactly the encoding wanted.
    (sectors & 0xff) as u8
}

/// The sectors an extent of `bytes` occupies, rounded up: what a transfer has to cover.
pub const fn sectors_for(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(SECTOR_BYTES as u64)
}

/// The byte offset of sector `lba` inside a linear image of the device.
///
/// The filesystem uses it for its own offsets, and the host tests use it to pretend a
/// `Vec<u8>` is a disk.
pub const fn byte_offset(lba: u64) -> u64 {
    lba * SECTOR_BYTES as u64
}

/// The bytes a span of sectors covers.
pub const fn span_bytes(sectors: u64) -> u64 {
    sectors * SECTOR_BYTES as u64
}

/// Whether a transfer of `buffer_len` bytes starting at `lba` stays inside a device of
/// `device_sectors` sectors. Shared by the driver and by the host tests, so "past the end"
/// means the same thing on both.
pub const fn transfer_fits(lba: u64, buffer_len: usize, device_sectors: u64) -> bool {
    if !buffer_len.is_multiple_of(SECTOR_BYTES) {
        return false;
    }
    let sectors = buffer_len as u64 / SECTOR_BYTES as u64;
    match lba.checked_add(sectors) {
        Some(end) => end <= device_sectors,
        None => false,
    }
}

/// A disk that is not there, so the boot can report that instead of failing.
///
/// The kernel has to boot on a machine with no disk - a laptop with the controller in
/// AHCI mode, a firmware that did not hand one over - and every later layer has to cope
/// with "no storage" as a state rather than an absence of code.
pub struct Absent;

impl Block for Absent {
    fn sectors(&self) -> u64 {
        0
    }

    fn read(&mut self, _lba: u64, _buffer: &mut [u8]) -> Result<(), DiskError> {
        Err(DiskError::NoDevice)
    }

    fn write(&mut self, _lba: u64, _source: &[u8]) -> Result<(), DiskError> {
        Err(DiskError::NoDevice)
    }
}

/// The driver itself, compiled only where port I/O exists.
#[cfg(target_arch = "x86_64")]
mod ata {
    use alloc::string::String;

    use super::{
        count_register, drive_head, lba28_sectors, model, status, transfer_fits, Block, DiskError,
        CHUNK_SECTORS, SECTOR_BYTES,
    };
    use crate::port::{inb, insw, io_wait, outb, outsw};

    /// The primary channel's command block and control block.
    const PRIMARY_IO: u16 = 0x1f0;
    const PRIMARY_CONTROL: u16 = 0x3f6;

    /// Registers inside the command block, added to its base.
    const DATA: u16 = 0;
    const SECTOR_COUNT: u16 = 2;
    const LBA_LOW: u16 = 3;
    const LBA_MID: u16 = 4;
    const LBA_HIGH: u16 = 5;
    const DRIVE_HEAD: u16 = 6;
    const COMMAND: u16 = 7;

    const READ_SECTORS: u8 = 0x20;
    const WRITE_SECTORS: u8 = 0x30;
    const IDENTIFY: u8 = 0xec;

    /// How many status polls before a command is declared stuck. A real drive answers in
    /// microseconds and QEMU answers immediately; anything near this count is a fault, and
    /// polling forever would turn one stuck command into a hung boot.
    const POLLS: u32 = 2_000_000;

    /// An ATA drive on the primary channel, addressed 28 bits at a time.
    ///
    /// One drive is claimed (the master) because that is what the test image attaches and
    /// what a serial-ATA bridge usually presents. Probing the slave costs another identify
    /// and adds a case nothing in this kernel can exercise.
    pub struct Pio {
        io: u16,
        control: u16,
        model: String,
        sectors: u64,
    }

    impl Pio {
        /// Finds the primary master, or `None` if nothing answers there.
        ///
        /// # Safety
        ///
        /// Must run before any other driver claims the primary channel, and only on a
        /// machine where that channel is an ATA controller - which is what the PC platform
        /// promises and what QEMU's `if=ide` provides.
        pub unsafe fn probe() -> Option<Self> {
            let io = PRIMARY_IO;
            let control = PRIMARY_CONTROL;
            // SAFETY: the caller vouched for the channel; every write here is a register
            // the ATA specification defines for exactly this purpose.
            unsafe {
                // Selecting the drive is also how a floating bus is detected: a channel
                // with nothing attached reads back zero from the sector-count and LBA
                // registers after the select.
                outb(io + DRIVE_HEAD, 0xa0);
                io_wait(control);
                outb(io + SECTOR_COUNT, 0);
                outb(io + LBA_LOW, 0);
                outb(io + LBA_MID, 0);
                outb(io + LBA_HIGH, 0);
                outb(io + COMMAND, IDENTIFY);
                if inb(io + COMMAND) == 0 {
                    return None;
                }
                // The signature check: a channel with a device that is not ATA (or no
                // device at all) leaves these two at zero.
                if inb(io + LBA_MID) != 0 || inb(io + LBA_HIGH) != 0 {
                    return None;
                }
            }
            let mut drive = Self {
                io,
                control,
                model: String::new(),
                sectors: 0,
            };
            // SAFETY: as above.
            unsafe {
                // A drive that reports `ERR` on identify is not one to use.
                if drive.wait_ready().is_err() {
                    return None;
                }
            }
            let mut identify = [0u16; 256];
            // SAFETY: as above; identify leaves 256 words in the data register.
            unsafe {
                for word in &mut identify {
                    *word = crate::port::inw(drive.io + DATA);
                }
            }
            if !super::supports_lba28(identify[49]) {
                return None;
            }
            let sectors = lba28_sectors(identify[60], identify[61]);
            if sectors == 0 {
                return None;
            }
            drive.model = model(&identify);
            drive.sectors = sectors;
            Some(drive)
        }

        /// The model string the drive reported, spaces trimmed.
        pub fn model(&self) -> &str {
            &self.model
        }

        /// Waits until the controller is not busy.
        ///
        /// # Safety
        ///
        /// The channel must be the one this driver probed.
        unsafe fn wait_ready(&self) -> Result<(), DiskError> {
            for _ in 0..POLLS {
                // SAFETY: reads the status register of the channel this driver owns.
                if unsafe { inb(self.io + COMMAND) } & status::BSY == 0 {
                    return Ok(());
                }
                // SAFETY: as above, for the alternate status register the delay needs.
                unsafe { io_wait(self.control) };
            }
            Err(DiskError::Timeout)
        }

        /// Waits until the controller has a sector ready to hand over, reporting a drive
        /// error instead of waiting for data that is never coming.
        ///
        /// # Safety
        ///
        /// As [`Self::wait_ready`].
        unsafe fn wait_drq(&self) -> Result<(), DiskError> {
            for _ in 0..POLLS {
                // SAFETY: as `wait_ready`.
                let status = unsafe { inb(self.io + COMMAND) };
                if status & status::BSY == 0 {
                    if status & (status::ERR | status::DF) != 0 {
                        return Err(DiskError::DeviceFault);
                    }
                    if status & status::DRQ != 0 {
                        return Ok(());
                    }
                }
                // SAFETY: as `wait_ready`.
                unsafe { io_wait(self.control) };
            }
            Err(DiskError::NoData)
        }

        /// Issues one command for `sectors` blocks at `lba`.
        ///
        /// # Safety
        ///
        /// As [`Self::wait_ready`], and the caller must follow with the data transfer the
        /// command asks for.
        unsafe fn command(&self, lba: u64, sectors: usize, command: u8) -> Result<(), DiskError> {
            // SAFETY: as `wait_ready`; the register order is the one the specification
            // gives for a 28-bit LBA command.
            unsafe {
                self.wait_ready()?;
                outb(self.io + DRIVE_HEAD, drive_head(lba, true));
                io_wait(self.control);
                outb(self.io + SECTOR_COUNT, count_register(sectors));
                outb(self.io + LBA_LOW, lba as u8);
                outb(self.io + LBA_MID, (lba >> 8) as u8);
                outb(self.io + LBA_HIGH, (lba >> 16) as u8);
                outb(self.io + COMMAND, command);
            }
            Ok(())
        }
    }

    impl Block for Pio {
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
            let mut offset = 0;
            let mut next = lba;
            while offset < buffer.len() {
                let sectors = (buffer.len() - offset)
                    .div_ceil(SECTOR_BYTES)
                    .min(CHUNK_SECTORS);
                // SAFETY: the channel is this driver's, `next` and `sectors` were checked
                // against the device's own size, and the command is a read.
                unsafe {
                    self.command(next, sectors, READ_SECTORS)?;
                }
                for sector in 0..sectors {
                    // SAFETY: as above; `wait_drq` is what makes the transfer that follows
                    // legal, and the slice is the destination for exactly this sector.
                    unsafe {
                        self.wait_drq()?;
                        let start = offset + sector * SECTOR_BYTES;
                        insw(self.io + DATA, &mut buffer[start..start + SECTOR_BYTES]);
                    }
                }
                offset += sectors * SECTOR_BYTES;
                next += sectors as u64;
            }
            Ok(())
        }

        fn write(&mut self, lba: u64, source: &[u8]) -> Result<(), DiskError> {
            if !transfer_fits(lba, source.len(), self.sectors) {
                return Err(DiskError::OutOfRange {
                    lba,
                    sectors: self.sectors,
                });
            }
            let mut offset = 0;
            let mut next = lba;
            while offset < source.len() {
                let sectors = (source.len() - offset)
                    .div_ceil(SECTOR_BYTES)
                    .min(CHUNK_SECTORS);
                // SAFETY: as `read`, with the write command.
                unsafe {
                    self.command(next, sectors, WRITE_SECTORS)?;
                }
                for sector in 0..sectors {
                    // SAFETY: as `read`, with the sector as the source.
                    unsafe {
                        self.wait_drq()?;
                        let start = offset + sector * SECTOR_BYTES;
                        outsw(self.io + DATA, &source[start..start + SECTOR_BYTES]);
                    }
                }
                // The controller raises `BSY` once the last word is in and drops it when
                // the drive has the data; a write that is not waited for here can be
                // overtaken by the next command issued to the same channel.
                // SAFETY: as `read`.
                unsafe {
                    self.wait_ready()?;
                }
                offset += sectors * SECTOR_BYTES;
                next += sectors as u64;
            }
            Ok(())
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use ata::Pio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drive_head_byte_carries_the_top_address_nibble() {
        // Small addresses: only the historical bits and the drive select.
        assert_eq!(drive_head(0, true), 0xe0);
        assert_eq!(drive_head(0, false), 0xf0);
        // A block address whose top nibble is not zero: the whole point of the function.
        assert_eq!(drive_head(0x1234_5678, true), 0xe2);
        assert_eq!(drive_head(0x0fff_ffff, true), 0xef);
        // The low 24 bits must not leak into the byte.
        assert_eq!(drive_head(0x00ff_ffff, true), 0xe0);
    }

    #[test]
    fn word_49_says_whether_lba28_is_there() {
        assert!(supports_lba28(1 << 9));
        assert!(supports_lba28(0xffff));
        assert!(!supports_lba28(0));
        assert!(!supports_lba28(1 << 8));
    }

    #[test]
    fn the_size_comes_from_words_60_and_61() {
        assert_eq!(lba28_sectors(0, 0), 0);
        assert_eq!(lba28_sectors(8192, 0), 8192);
        assert_eq!(lba28_sectors(0, 1), 65536);
        assert_eq!(lba28_sectors(0xffff, 0xffff), 0xffff_ffff);
    }

    #[test]
    fn the_model_string_is_byte_swapped_and_trimmed() {
        let mut identify = [0u16; 256];
        // "QEMU HARDDISK" as big-endian word pairs, space padded to 40 characters.
        let text = b"QEMU HARDDISK                           ";
        for (index, pair) in text.chunks_exact(2).enumerate() {
            identify[27 + index] = u16::from_be_bytes([pair[0], pair[1]]);
        }
        assert_eq!(model(&identify), "QEMU HARDDISK");
    }

    #[test]
    fn a_model_with_no_trailing_space_keeps_its_last_character() {
        let mut identify = [0u16; 256];
        let text = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCD";
        for (index, pair) in text.chunks_exact(2).enumerate() {
            identify[27 + index] = u16::from_be_bytes([pair[0], pair[1]]);
        }
        assert_eq!(model(&identify), "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCD");
    }

    #[test]
    fn the_count_register_encodes_256_as_zero() {
        assert_eq!(count_register(1), 1);
        assert_eq!(count_register(255), 255);
        assert_eq!(count_register(256), 0);
    }

    #[test]
    fn extents_round_up_to_whole_sectors() {
        assert_eq!(sectors_for(0), 0);
        assert_eq!(sectors_for(1), 1);
        assert_eq!(sectors_for(512), 1);
        assert_eq!(sectors_for(513), 2);
        assert_eq!(span_bytes(2), 1024);
        assert_eq!(byte_offset(2), 1024);
    }

    #[test]
    fn a_transfer_past_the_end_is_refused() {
        assert!(transfer_fits(0, 512, 1));
        assert!(transfer_fits(0, 1024, 2));
        assert!(!transfer_fits(1, 1024, 2));
        assert!(!transfer_fits(2, 512, 2));
        // Not a whole number of sectors.
        assert!(!transfer_fits(0, 511, 2));
        // An address space that would wrap.
        assert!(!transfer_fits(u64::MAX, 512, u64::MAX));
    }

    #[test]
    fn the_absent_disk_refuses_everything() {
        let mut disk = Absent;
        let mut buffer = [0u8; 512];
        assert_eq!(disk.sectors(), 0);
        assert_eq!(disk.read(0, &mut buffer), Err(DiskError::NoDevice));
        assert_eq!(disk.write(0, &buffer), Err(DiskError::NoDevice));
    }
}
