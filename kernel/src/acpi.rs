//! ACPI, as far as this kernel needs it: how many CPUs the machine has, and whether its
//! interrupt controller is the pair of 8259s the PIC driver talks to or an IOAPIC.
//!
//! # What is here and what is not
//!
//! There is no AML interpreter and no attempt to be a general ACPI implementation. [`discover`]
//! finds the RSDP, walks the root table, and reads one table - the MADT - for processor entries,
//! the IOAPIC's address and the interrupt source overrides. That is exactly the set of facts the
//! M10 report needs to be true rather than assumed.
//!
//! # Why it takes a [`Phys`]
//!
//! The tables live at physical addresses the firmware chose, and the kernel reaches physical
//! memory through the bootloader's window, which is a `main.rs` detail. Rather than teach this
//! module about that window, everything goes through one method - copy `len` bytes from a
//! physical address - so the host tests can hand it a slice of invented memory and check what it
//! makes of it. The same code then reads the real tables at boot.

/// Copies bytes out of physical memory.
pub trait Phys {
    /// Copies `len` bytes from physical `address` into `out`. `false` means "not readable",
    /// which is a normal answer for a scan that is looking for something that is not there.
    fn read(&mut self, address: u64, out: &mut [u8]) -> bool;
}

/// The eight bytes that start an RSDP.
pub const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";

/// The shortest an RSDP can be: the ACPI 1.0 structure.
pub const RSDP_BYTES: usize = 20;

/// The ACPI 2.0 RSDP, which adds an extended checksum and a 64-bit root table address.
pub const RSDP_V2_BYTES: usize = 36;

/// The header every ACPI table starts with.
pub const HEADER_BYTES: usize = 36;

/// The largest table this module will read. The MADT on a machine with many CPUs is the only
/// one that gets close, and a table whose length field claims more than this is refused rather
/// than trusted with an allocation.
const MAX_TABLE_BYTES: usize = 8 * 1024;

/// How many tables the root table may name. A firmware with more than this names tables this
/// kernel does not read; the count is reported rather than the names.
const MAX_TABLES: usize = 32;

/// Where the BIOS data area keeps the extended BIOS data area's segment.
const EBDA_POINTER: u64 = 0x40e;

/// The start and end of the region ACPI 1.0 says to search for an RSDP.
const BIOS_AREA: (u64, u64) = (0x000e_0000, 0x0010_0000);

/// Why the firmware's tables were not usable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No `RSD PTR ` with a valid checksum in the BIOS area or the extended BIOS data area.
    NoRsdp,
    /// An RSDP whose checksum does not cover it.
    BadRsdpChecksum(u64),
    /// The root table is not an `RSDT` or an `XSDT`, or fails its own checksum.
    BadRoot,
    /// The MADT is not an `APIC` table.
    BadMadt,
    /// A table claimed a length this module will not read.
    TooLarge(u32),
}

/// What the firmware says about this machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Report {
    /// The physical address the RSDP was found at.
    pub rsdp: u64,
    /// The ACPI revision the RSDP declares: 0 and 1 mean the root is an `RSDT`, 2 or more an
    /// `XSDT`.
    pub revision: u8,
    /// The table signatures the root table points at, in order and up to [`MAX_TABLES`].
    pub tables: alloc::vec::Vec<[u8; 4]>,
    /// Processor entries in the MADT, and how many of them are enabled.
    pub cpus: usize,
    pub enabled_cpus: usize,
    /// The IOAPIC's physical address, if the MADT describes one.
    pub ioapic: Option<u32>,
    /// Interrupt source overrides: the entries that say a legacy IRQ does not arrive on the
    /// vector its number suggests. Every one of them is a reason the PIC driver's mapping is
    /// not the whole truth, which is why they are counted.
    pub overrides: usize,
    /// Whether the address came from the caller's hint rather than from the two places ACPI 1.0
    /// says to search. A UEFI platform publishes the address in its configuration table and may
    /// keep nothing in the legacy areas at all, so which of the two happened is worth reporting.
    pub hinted: bool,
}

/// Finds the firmware's tables and reads what they say.
///
/// `hint` is an address the caller already believes holds an RSDP - the bootloader reads it out
/// of the UEFI configuration table, where a UEFI machine publishes it and where no legacy scan
/// would find it. A hint that does not validate is not an error: the search below still runs,
/// because a stale or wrong hint is exactly the case the legacy path exists for.
pub fn discover(phys: &mut impl Phys, hint: Option<u64>) -> Result<Report, Error> {
    let hinted = hint.and_then(|address| read_rsdp_at(phys, address).map(|rsdp| (address, rsdp)));
    let (rsdp_address, rsdp, hinted) = match hinted {
        Some((address, rsdp)) => (address, rsdp, true),
        None => {
            let (address, rsdp) = find_rsdp(phys)?;
            (address, rsdp, false)
        }
    };
    let root = read_root(phys, &rsdp)?;
    let mut report = Report {
        rsdp: rsdp_address,
        revision: rsdp.revision,
        tables: alloc::vec::Vec::new(),
        cpus: 0,
        enabled_cpus: 0,
        ioapic: None,
        overrides: 0,
        hinted,
    };
    for address in root {
        let mut header = [0u8; HEADER_BYTES];
        if !phys.read(address, &mut header) {
            continue;
        }
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let mut signature = [0u8; 4];
        signature.copy_from_slice(&header[0..4]);
        if report.tables.len() < MAX_TABLES {
            report.tables.push(signature);
        }
        if &signature != b"APIC" {
            continue;
        }
        let length = length as usize;
        if length > MAX_TABLE_BYTES {
            return Err(Error::TooLarge(length as u32));
        }
        let mut table = alloc::vec![0u8; length];
        if !phys.read(address, &mut table) {
            continue;
        }
        if !valid_table(&table) {
            // Not fatal: a table whose checksum is wrong is a firmware bug this kernel should
            // report rather than act on, and the rest of the report is still true.
            continue;
        }
        let madt = parse_madt(&table)?;
        report.cpus = madt.cpus;
        report.enabled_cpus = madt.enabled;
        report.ioapic = madt.ioapic;
        report.overrides = madt.overrides;
    }
    Ok(report)
}

/// A parsed RSDP: what it declares and where its root table is.
#[derive(Clone, Copy, Debug)]
struct Rsdp {
    revision: u8,
    /// The `RSDT` (4-byte entries) or `XSDT` (8-byte entries) address.
    root: u64,
}

/// Reads and validates an RSDP at an address the caller already believes holds one.
fn read_rsdp_at(phys: &mut impl Phys, address: u64) -> Option<Rsdp> {
    let mut bytes = [0u8; RSDP_V2_BYTES];
    if !phys.read(address, &mut bytes) {
        return None;
    }
    parse_rsdp(&bytes)
}

/// Walks the two places ACPI 1.0 says an RSDP can be, in the order it says to look.
fn find_rsdp(phys: &mut impl Phys) -> Result<(u64, Rsdp), Error> {
    // The extended BIOS data area first: the segment in the BDA points at it, and a valid
    // segment is below the video memory hole.
    let mut word = [0u8; 2];
    if phys.read(EBDA_POINTER, &mut word) {
        let base = u64::from(u16::from_le_bytes(word)) << 4;
        if (0x8000..0x000a_0000).contains(&base) {
            if let Some(found) = scan(phys, base, 1024)? {
                return Ok(found);
            }
        }
    }
    // Then the BIOS area itself, sixteen bytes at a time: the RSDP is 16-byte aligned by the
    // specification, and a scan that stepped by one would find the same structure four times.
    match scan(phys, BIOS_AREA.0, BIOS_AREA.1 - BIOS_AREA.0)? {
        Some(found) => Ok(found),
        None => Err(Error::NoRsdp),
    }
}

/// Searches `[base, base + len)` for an RSDP and returns the first one that validates.
fn scan(phys: &mut impl Phys, base: u64, len: u64) -> Result<Option<(u64, Rsdp)>, Error> {
    let mut signature = [0u8; 8];
    let mut bytes = [0u8; RSDP_V2_BYTES];
    let mut offset = 0u64;
    while offset + RSDP_BYTES as u64 <= len {
        let address = base + offset;
        if phys.read(address, &mut signature) && signature == RSDP_SIGNATURE {
            if !phys.read(address, &mut bytes) {
                return Err(Error::BadRsdpChecksum(address));
            }
            match parse_rsdp(&bytes) {
                Some(rsdp) => return Ok(Some((address, rsdp))),
                None => return Err(Error::BadRsdpChecksum(address)),
            }
        }
        offset += 16;
    }
    Ok(None)
}

/// Validates an RSDP and pulls the two facts in it out.
fn parse_rsdp(bytes: &[u8]) -> Option<Rsdp> {
    if bytes.len() < RSDP_BYTES || bytes[0..8] != RSDP_SIGNATURE {
        return None;
    }
    if sum(&bytes[..RSDP_BYTES]) != 0 {
        return None;
    }
    let revision = bytes[15];
    if revision >= 2 {
        // The extended structure has its own checksum over the extra sixteen bytes, and a
        // 64-bit root table address that a 32-bit one cannot express.
        if bytes.len() < RSDP_V2_BYTES || sum(&bytes[..RSDP_V2_BYTES]) != 0 {
            return None;
        }
        let root = u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
        ]);
        return Some(Rsdp { revision, root });
    }
    let root = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    Some(Rsdp {
        revision,
        root: u64::from(root),
    })
}

/// Reads the root table and returns the addresses it names.
fn read_root(phys: &mut impl Phys, rsdp: &Rsdp) -> Result<alloc::vec::Vec<u64>, Error> {
    let mut header = [0u8; HEADER_BYTES];
    if !phys.read(rsdp.root, &mut header) {
        return Err(Error::BadRoot);
    }
    let signature = &header[0..4];
    let entry_bytes = match signature {
        b"XSDT" => 8,
        b"RSDT" => 4,
        _ => return Err(Error::BadRoot),
    };
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if length > MAX_TABLE_BYTES {
        return Err(Error::TooLarge(length as u32));
    }
    if length < HEADER_BYTES || !(length - HEADER_BYTES).is_multiple_of(entry_bytes) {
        return Err(Error::BadRoot);
    }
    let mut table = alloc::vec![0u8; length];
    if !phys.read(rsdp.root, &mut table) || !valid_table(&table) {
        return Err(Error::BadRoot);
    }
    let mut addresses = alloc::vec::Vec::new();
    let mut offset = HEADER_BYTES;
    while offset + entry_bytes <= length && addresses.len() < MAX_TABLES {
        let address = if entry_bytes == 8 {
            u64::from_le_bytes([
                table[offset],
                table[offset + 1],
                table[offset + 2],
                table[offset + 3],
                table[offset + 4],
                table[offset + 5],
                table[offset + 6],
                table[offset + 7],
            ])
        } else {
            u64::from(u32::from_le_bytes([
                table[offset],
                table[offset + 1],
                table[offset + 2],
                table[offset + 3],
            ]))
        };
        if address != 0 {
            addresses.push(address);
        }
        offset += entry_bytes;
    }
    Ok(addresses)
}

/// What the MADT says about processors and interrupts.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Madt {
    cpus: usize,
    enabled: usize,
    ioapic: Option<u32>,
    overrides: usize,
}

/// The MADT's own header: the ACPI header plus the local APIC's address and flags.
const MADT_HEADER_BYTES: usize = HEADER_BYTES + 8;

/// Reads the MADT's entry list.
///
/// The entries are a chain of `(type, length)` records rather than a fixed array, which is why
/// the loop is a `while` over a length and not an index into a slice of structs: a firmware that
/// writes a length of zero would otherwise be an infinite loop, and one that writes a length
/// past the end of the table would be an out-of-bounds read.
fn parse_madt(bytes: &[u8]) -> Result<Madt, Error> {
    if bytes.len() < MADT_HEADER_BYTES || &bytes[0..4] != b"APIC" {
        return Err(Error::BadMadt);
    }
    let mut madt = Madt::default();
    let mut offset = MADT_HEADER_BYTES;
    while offset + 2 <= bytes.len() {
        let kind = bytes[offset];
        let length = bytes[offset + 1] as usize;
        if length < 2 || offset + length > bytes.len() {
            // A malformed entry ends the walk rather than being guessed at: everything before it
            // was still read from a table whose checksum passed, and everything after it is
            // unreachable by construction.
            break;
        }
        let entry = &bytes[offset..offset + length];
        match kind {
            // Processor local APIC: flags in the low bit of the word at offset 4 say whether the
            // firmware enabled it. A processor the firmware disabled is not one this kernel can
            // start, so it counts as present and not as available.
            0 if length >= 8 => {
                madt.cpus += 1;
                if u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]) & 1 == 1 {
                    madt.enabled += 1;
                }
            }
            // Processor local x2APIC: the same thing with a 32-bit id instead of an 8-bit one.
            9 if length >= 16 => {
                madt.cpus += 1;
                if u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) & 1 == 1 {
                    madt.enabled += 1;
                }
            }
            // IOAPIC: the address is a 32-bit physical address at offset 4.
            1 if length >= 12 => {
                if madt.ioapic.is_none() {
                    madt.ioapic =
                        Some(u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]));
                }
            }
            // Interrupt source override: an ISA IRQ that does not arrive on the GSI its number
            // suggests. The first thing that stops the PIC's `irq + 0x20` mapping being the whole
            // truth, so the count is reported.
            2 if length >= 10 => madt.overrides += 1,
            _ => {}
        }
        offset += length;
    }
    Ok(madt)
}

/// The sum of every byte, as the firmware computes it: a table is valid when this is zero.
fn sum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |total, byte| total.wrapping_add(*byte))
}

/// Whether a table's own checksum covers it.
fn valid_table(bytes: &[u8]) -> bool {
    sum(bytes) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A machine whose physical memory this test decides, so the tables can be any shape.
    #[derive(Default)]
    struct Ram {
        regions: vec::Vec<(u64, vec::Vec<u8>)>,
    }

    impl Ram {
        fn put(&mut self, address: u64, bytes: &[u8]) {
            for (base, existing) in &self.regions {
                let overlaps =
                    address < base + existing.len() as u64 && *base < address + bytes.len() as u64;
                assert!(
                    !overlaps,
                    "two tables placed at once: {address:#x} overlaps {base:#x}"
                );
            }
            self.regions.push((address, bytes.to_vec()));
        }
    }

    impl Phys for Ram {
        fn read(&mut self, address: u64, out: &mut [u8]) -> bool {
            for (base, bytes) in &self.regions {
                if address < *base {
                    continue;
                }
                let start = (address - base) as usize;
                if start + out.len() <= bytes.len() {
                    out.copy_from_slice(&bytes[start..start + out.len()]);
                    return true;
                }
            }
            false
        }
    }

    /// An ACPI table with its length and checksum filled in.
    fn table(signature: &[u8; 4], body: &[u8]) -> vec::Vec<u8> {
        let mut bytes = vec::Vec::new();
        bytes.extend_from_slice(signature);
        bytes.extend_from_slice(&((HEADER_BYTES + body.len()) as u32).to_le_bytes());
        bytes.push(2);
        bytes.push(0);
        bytes.extend_from_slice(b"GREYOS");
        bytes.extend_from_slice(b"grey-os ");
        // OEM revision, creator id and creator revision: four zeroes each.
        bytes.extend_from_slice(&[0u8; 12]);
        bytes.extend_from_slice(body);
        fix(&mut bytes);
        bytes
    }

    /// Sets the checksum byte so the table sums to zero.
    fn fix(bytes: &mut [u8]) {
        bytes[9] = 0;
        bytes[9] = 0u8.wrapping_sub(sum(bytes));
    }

    fn processor(apic_id: u8, enabled: bool) -> vec::Vec<u8> {
        let mut entry = vec![0u8, 8, apic_id, 0];
        entry.extend_from_slice(&u32::from(enabled).to_le_bytes());
        entry
    }

    fn x2apic(x2apic_id: u32, enabled: bool) -> vec::Vec<u8> {
        let mut entry = vec![9u8, 16, 0, 0];
        entry.extend_from_slice(&x2apic_id.to_le_bytes());
        entry.extend_from_slice(&u32::from(enabled).to_le_bytes());
        entry.extend_from_slice(&1u32.to_le_bytes());
        entry
    }

    fn ioapic_entry(address: u32) -> vec::Vec<u8> {
        let mut entry = vec![1u8, 12, 0, 0];
        entry.extend_from_slice(&address.to_le_bytes());
        entry.extend_from_slice(&[0u8; 4]);
        entry
    }

    fn source_override() -> vec::Vec<u8> {
        // Bus 0, IRQ 0, GSI 2: the classic "the timer is not on IRQ0" override.
        vec![2u8, 10, 0, 0, 0, 2, 0, 0, 0, 0]
    }

    /// A MADT whose body is the local APIC's address and flags followed by `entries`.
    fn madt(entries: &[vec::Vec<u8>]) -> vec::Vec<u8> {
        let mut body = vec::Vec::new();
        body.extend_from_slice(&0xfee0_0000u32.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        for entry in entries {
            body.extend_from_slice(entry);
        }
        table(b"APIC", &body)
    }

    /// An RSDP declaring `revision`, pointing at `rsdt` or `xsdt` as the revision chooses.
    fn rsdp(revision: u8, rsdt: u32, xsdt: u64) -> vec::Vec<u8> {
        let mut bytes = vec![0u8; RSDP_V2_BYTES];
        bytes[0..8].copy_from_slice(&RSDP_SIGNATURE);
        bytes[9..15].copy_from_slice(b"GREYOS");
        bytes[15] = revision;
        bytes[16..20].copy_from_slice(&rsdt.to_le_bytes());
        bytes[20..24].copy_from_slice(&(RSDP_V2_BYTES as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&xsdt.to_le_bytes());
        bytes[8] = 0;
        bytes[8] = 0u8.wrapping_sub(sum(&bytes[..RSDP_BYTES]));
        bytes[32] = 0u8.wrapping_sub(sum(&bytes[..RSDP_V2_BYTES]));
        bytes
    }

    /// The root table (`XSDT` for a 64-bit list, `RSDT` for a 32-bit one).
    fn root(xsdt: bool, tables: &[u64]) -> vec::Vec<u8> {
        let mut body = vec::Vec::new();
        for address in tables {
            if xsdt {
                body.extend_from_slice(&address.to_le_bytes());
            } else {
                body.extend_from_slice(&(*address as u32).to_le_bytes());
            }
        }
        if xsdt {
            table(b"XSDT", &body)
        } else {
            table(b"RSDT", &body)
        }
    }

    /// The machine the boot log in ADR-037 describes: four processors, one of them disabled by
    /// the firmware, an IOAPIC, and the timer's override.
    fn four_cpus() -> Ram {
        let mut ram = Ram::default();
        ram.put(
            0x0000_2000,
            &madt(&[
                processor(0, true),
                processor(1, true),
                processor(2, true),
                processor(3, false),
                ioapic_entry(0xfee0_0000),
                source_override(),
            ]),
        );
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x000e_0000, &rsdp(2, 0, 0x0000_1000));
        ram
    }

    #[test]
    fn four_processors_and_an_ioapic_are_read_out_of_the_tables() {
        let report = discover(&mut four_cpus(), None).expect("the tables are valid");
        assert_eq!(report.rsdp, 0x000e_0000);
        assert_eq!(report.revision, 2);
        assert_eq!(report.tables, vec![[b'A', b'P', b'I', b'C']]);
        assert_eq!(report.cpus, 4);
        // One processor's enabled flag is clear: present, but not one this kernel could use.
        assert_eq!(report.enabled_cpus, 3);
        assert_eq!(report.ioapic, Some(0xfee0_0000));
        // The timer on GSI 2 rather than IRQ 0 - why the PIC's mapping is not the whole truth.
        assert_eq!(report.overrides, 1);
    }

    #[test]
    fn a_1_0_rsdp_reads_the_32_bit_root_table() {
        let mut ram = Ram::default();
        ram.put(
            0x0000_2000,
            &madt(&[processor(0, true), processor(1, true)]),
        );
        ram.put(0x0000_1000, &root(false, &[0x0000_2000]));
        ram.put(0x000e_0000, &rsdp(0, 0x0000_1000, 0));
        let report = discover(&mut ram, None).expect("a v1 machine is still a machine");
        assert_eq!(report.revision, 0);
        assert_eq!(report.cpus, 2);
        assert_eq!(report.enabled_cpus, 2);
        assert_eq!(report.ioapic, None);
    }

    #[test]
    fn the_extended_bios_data_area_is_searched_before_the_bios_area() {
        // The BDA's word at 0x40e is a segment, so 0x0900 is the EBDA at 0x9000.
        let mut ram = Ram::default();
        ram.put(0x40e, &0x0900u16.to_le_bytes());
        ram.put(0x0000_2000, &madt(&[processor(0, true)]));
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x0000_9000, &rsdp(2, 0, 0x0000_1000));
        // Nothing at 0xe0000 at all: a scan that looked there only would find no RSDP.
        let report = discover(&mut ram, None).expect("the EBDA holds the rsdp");
        assert_eq!(report.rsdp, 0x0000_9000);
        assert_eq!(report.cpus, 1);
    }

    #[test]
    fn a_machine_with_no_rsdp_says_so_rather_than_guessing() {
        let mut ram = Ram::default();
        assert_eq!(discover(&mut ram, None).err(), Some(Error::NoRsdp));
    }

    #[test]
    fn the_bootloaders_hint_finds_a_table_no_legacy_scan_would_reach() {
        // What QEMU with OVMF does: the tables live high, in reserved RAM, and both legacy
        // areas hold nothing that looks like an RSDP.
        let mut ram = Ram::default();
        ram.put(
            0x0000_2000,
            &madt(&[processor(0, true), processor(1, true)]),
        );
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x07fe_0000, &rsdp(2, 0, 0x0000_1000));
        assert_eq!(discover(&mut ram, None).err(), Some(Error::NoRsdp));
        let report = discover(&mut ram, Some(0x07fe_0000)).expect("the hint is the table");
        assert_eq!(report.rsdp, 0x07fe_0000);
        assert!(report.hinted);
        assert_eq!(report.cpus, 2);
    }

    #[test]
    fn a_hint_that_does_not_validate_falls_back_to_the_search() {
        let mut ram = Ram::default();
        ram.put(0x0000_2000, &madt(&[processor(0, true)]));
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x000e_0000, &rsdp(2, 0, 0x0000_1000));
        // A stale hint points at nothing readable; the legacy search still runs.
        let report = discover(&mut ram, Some(0x0700_0000)).expect("the scan finds it");
        assert_eq!(report.rsdp, 0x000e_0000);
        assert!(!report.hinted);
        // And a hint at real memory that is not an RSDP is refused the same way.
        ram.put(0x0700_0000, &[0u8; RSDP_V2_BYTES]);
        let report = discover(&mut ram, Some(0x0700_0000)).expect("the scan finds it");
        assert_eq!(report.rsdp, 0x000e_0000);
        assert!(!report.hinted);
    }

    #[test]
    fn an_rsdp_whose_checksum_is_wrong_is_refused_by_its_address() {
        let mut ram = Ram::default();
        let mut bytes = rsdp(2, 0, 0x0000_1000);
        bytes[9..15].copy_from_slice(b"BROKEN");
        ram.put(0x000e_0000, &bytes);
        assert_eq!(
            discover(&mut ram, None).err(),
            Some(Error::BadRsdpChecksum(0x000e_0000))
        );
    }

    #[test]
    fn a_root_table_that_is_not_one_is_refused() {
        let mut ram = Ram::default();
        ram.put(0x0000_1000, b"not a table at all, just bytes");
        ram.put(0x000e_0000, &rsdp(2, 0, 0x0000_1000));
        assert_eq!(discover(&mut ram, None).err(), Some(Error::BadRoot));
    }

    #[test]
    fn a_table_that_is_not_a_madt_is_counted_and_left_alone() {
        let mut ram = Ram::default();
        // A FACP where the MADT would be: named in the report, parsed by nothing.
        ram.put(0x0000_2000, &table(b"FACP", &[0u8; 16]));
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x000e_0000, &rsdp(2, 0, 0x0000_1000));
        let report = discover(&mut ram, None).expect("the tables are valid");
        assert_eq!(report.tables, vec![[b'F', b'A', b'C', b'P']]);
        assert_eq!(report.cpus, 0);
    }

    #[test]
    fn a_madt_with_a_broken_checksum_is_reported_and_not_read() {
        let mut ram = Ram::default();
        let mut bytes = madt(&[processor(0, true)]);
        bytes[9] = bytes[9].wrapping_add(1);
        ram.put(0x0000_2000, &bytes);
        ram.put(0x0000_1000, &root(true, &[0x0000_2000]));
        ram.put(0x000e_0000, &rsdp(2, 0, 0x0000_1000));
        let report = discover(&mut ram, None).expect("one bad table is not a bad machine");
        assert_eq!(report.tables, vec![[b'A', b'P', b'I', b'C']]);
        assert_eq!(report.cpus, 0);
    }

    #[test]
    fn an_entry_with_a_zero_length_ends_the_walk_instead_of_looping() {
        let mut bytes = madt(&[processor(0, true), processor(1, true)]);
        // Append an entry whose length byte is zero: a `while` over a length would never move.
        bytes.extend_from_slice(&[0u8, 0]);
        // The table's length field still has to cover what was appended, and so does its
        // checksum, or the walk would never be reached.
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        fix(&mut bytes);
        let madt = parse_madt(&bytes).expect("two good entries and a broken one");
        assert_eq!(madt.cpus, 2);
        assert_eq!(madt.enabled, 2);
    }

    #[test]
    fn an_x2apic_entry_counts_as_a_processor() {
        let bytes = madt(&[processor(0, true), x2apic(260, false)]);
        let madt = parse_madt(&bytes).expect("the table parses");
        assert_eq!(madt.cpus, 2);
        assert_eq!(madt.enabled, 1);
    }

    #[test]
    fn something_that_is_not_a_madt_is_not_parsed_as_one() {
        assert_eq!(parse_madt(b"FACP"), Err(Error::BadMadt));
        assert_eq!(parse_madt(&table(b"FACP", &[0u8; 8])), Err(Error::BadMadt));
        // An `APIC` table too short to hold its own header is not a MADT either.
        assert_eq!(parse_madt(b"APIC\x00\x00\x00\x00"), Err(Error::BadMadt));
    }
}
