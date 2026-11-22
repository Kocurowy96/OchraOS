// SPDX-License-Identifier: GPL-2.0-only

//! Reading ELF executables (`man 5 elf`): the file header and the program headers, which
//! say which parts of the file go where in memory ("segments").
//!
//! Two kinds are accepted, both statically linked (a dynamic linker comes later):
//! - `ET_EXEC`, a classic executable for one fixed address;
//! - `ET_DYN` without an interpreter, a static-PIE executable: it can run at any address
//!   (the loader picks a random one, ASLR) and relocates itself at start (musl's `rcrt1.o`).
//!
//! Every number is checked against the file's size, so a broken or hostile file gives
//! `ENOEXEC`, never a kernel crash.

use alloc::vec::Vec;

use crate::errno::{ENOEXEC, Errno};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `ET_EXEC`: loaded at the addresses in the file.
    Fixed,
    /// `ET_DYN`: loaded anywhere, at a page-aligned offset ("load bias").
    PositionIndependent,
}

/// A `PT_LOAD` segment: `file_size` bytes from `offset` in the file go to `address`; the
/// rest up to `memory_size` is zero (`.bss`).
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub offset: u64,
    pub address: u64,
    pub file_size: u64,
    pub memory_size: u64,
    pub writable: bool,
    pub executable: bool,
}

pub struct Elf {
    pub kind: Kind,
    pub entry: u64,
    /// How many program headers there are, and their address in memory if a segment loads
    /// them (needed for `AT_PHDR`).
    pub header_count: u16,
    pub header_address: Option<u64>,
    /// The `PT_LOAD` segments, in ascending address order.
    pub segments: Vec<Segment>,
}

const HEADER_SIZE: usize = 64;
/// Size of one program header for 64-bit ELF (also `AT_PHENT`).
pub const PROGRAM_HEADER_SIZE: u16 = 56;

const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// More program headers than this is not a real program (Linux allows 64 KiB of them).
const MAX_HEADERS: u16 = 128;
const PAGE: u64 = 4096;

pub fn parse(file: &[u8]) -> Result<Elf, Errno> {
    let header = file.get(..HEADER_SIZE).ok_or(ENOEXEC)?;
    // Magic, 64-bit (class 2), little-endian (data 1), ELF version 1.
    if header[..4] != *b"\x7fELF" || header[4] != 2 || header[5] != 1 || header[6] != 1 {
        return Err(ENOEXEC);
    }
    let kind = match read_u16(header, 16) {
        ET_EXEC => Kind::Fixed,
        ET_DYN => Kind::PositionIndependent,
        _ => return Err(ENOEXEC),
    };
    if read_u16(header, 18) != EM_X86_64 || read_u16(header, 54) != PROGRAM_HEADER_SIZE {
        return Err(ENOEXEC);
    }
    let entry = read_u64(header, 24);
    let header_offset = read_u64(header, 32);
    let header_count = read_u16(header, 56);
    if header_count == 0 || header_count > MAX_HEADERS {
        return Err(ENOEXEC);
    }
    let table_size = u64::from(header_count) * u64::from(PROGRAM_HEADER_SIZE);
    let table = usize::try_from(header_offset)
        .ok()
        .and_then(|start| file.get(start..start.checked_add(table_size as usize)?))
        .ok_or(ENOEXEC)?;

    let mut segments: Vec<Segment> = Vec::new();
    let mut header_address = None;
    for entry in table.chunks_exact(usize::from(PROGRAM_HEADER_SIZE)) {
        let kind = read_u32(entry, 0);
        let flags = read_u32(entry, 4);
        let offset = read_u64(entry, 8);
        let address = read_u64(entry, 16);
        let file_size = read_u64(entry, 32);
        let memory_size = read_u64(entry, 40);
        match kind {
            PT_INTERP => {
                log::debug!("exec: dynamically linked programs are not supported yet");
                return Err(ENOEXEC);
            }
            PT_PHDR => header_address = Some(address),
            PT_LOAD => {
                let segment = Segment {
                    offset,
                    address,
                    file_size,
                    memory_size,
                    writable: flags & PF_W != 0,
                    executable: flags & PF_X != 0,
                };
                check_segment(&segment, file.len() as u64)?;
                // Segments must come in ascending order and not overlap (except sharing a
                // page, which the loader handles).
                if segments
                    .last()
                    .is_some_and(|previous| previous.address + previous.memory_size > address)
                {
                    return Err(ENOEXEC);
                }
                segments.push(segment);
            }
            _ => {}
        }
    }
    if segments.is_empty() {
        return Err(ENOEXEC);
    }
    // Without PT_PHDR, the headers are in memory if a segment loads that part of the file.
    let header_address = header_address.or_else(|| {
        segments.iter().find_map(|segment| {
            let inside = header_offset >= segment.offset
                && header_offset + table_size <= segment.offset + segment.file_size;
            inside.then(|| segment.address + (header_offset - segment.offset))
        })
    });
    Ok(Elf {
        kind,
        entry,
        header_count,
        header_address,
        segments,
    })
}

fn check_segment(segment: &Segment, file_length: u64) -> Result<(), Errno> {
    let in_file = segment
        .offset
        .checked_add(segment.file_size)
        .is_some_and(|end| end <= file_length);
    let fits = segment
        .address
        .checked_add(segment.memory_size)
        .is_some_and(|end| end <= ochra_frame::mm::user::USER_END);
    // The file offset and the address must be the same distance into a page, so the file
    // could be mapped page by page.
    let congruent = segment.offset % PAGE == segment.address % PAGE;
    // W^X: no segment may be both writable and executable (docs/decisions.md).
    let w_xor_x = !(segment.writable && segment.executable);
    if in_file && fits && congruent && w_xor_x && segment.file_size <= segment.memory_size {
        Ok(())
    } else {
        Err(ENOEXEC)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}
