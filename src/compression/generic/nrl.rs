/*
 * Copyright 2021-2022 Capypara and the SkyTemple Contributors
 *
 * This file is part of SkyTemple.
 *
 * SkyTemple is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * SkyTemple is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with SkyTemple.  If not, see <https://www.gnu.org/licenses/>.
 */

use std::io::Cursor;
use bytes::{Buf, BufMut, Bytes, BytesMut};

// Operations are encoded in command bytes (CMD):
const CMD_ZERO_OUT: u8      = 0x80;  // All values below
const CMD_FILL_OUT: u8      = 0x80;  // All values equal/above until next
const CMD_COPY_BYTES: u8    = 0xC0;  // All values equal/above

// How much bytes we look ahead for at most
const NRL_LOOKAHEAD_ZERO_MAX_BYTES: u8 = 127;
const NRL_LOOKAHEAD_FILL_MAX_BYTES: u8 = 63;
const NRL_LOOKAHEAD_COPY_BYTES_MAX_BYTES: u8 = 63;
// How often a byte needs to repeat for ZERO_OUT and FILL_OUT
const NRL_MIN_SEQ_LEN: usize = 3;

pub(crate) trait NrlRead<U> {
    // Returns the next byte
    fn nrl_get(&mut self) -> u8;
    // Advance the internal cursor as if n nrl_get calls were done.
    fn nrl_advance(&mut self, n: usize);
    fn has_remaining(&self) -> bool;
}

impl<T> NrlRead<T> for Cursor<T> where T: AsRef<[u8]> {
    fn nrl_get(&mut self) -> u8 {
        self.get_u8()
    }
    fn nrl_advance(&mut self, n: usize) {
        self.advance(n)
    }
    fn has_remaining(&self) -> bool {
        Buf::has_remaining(self)
    }
}

pub(crate) trait NrlWrite {
    fn nrl_put(&mut self, b: u8);
}

impl<T> NrlWrite for T where T: BufMut {
    fn nrl_put(&mut self, b: u8) {
        self.put_u8(b)
    }
}

pub(crate) fn compression_step<T, U, W>(
    decompressed_data: &mut T, compressed_data: &mut W
) where
    T: NrlRead<U> + Clone,
    U: AsRef<[u8]>,
    W: BufMut
{
    let sequence = _look_ahead_byte_sequence(decompressed_data);

    if sequence.len() > NRL_MIN_SEQ_LEN {
        // CMD_COPY_BYTES
        // Advance the "real" cursor too
        decompressed_data.nrl_advance(sequence.len());
        compressed_data.put_u8(CMD_COPY_BYTES + (sequence.len() as u8 - 1)); // cmd byte
        compressed_data.put(sequence);
    } else {
        let current_byte = decompressed_data.nrl_get();
        let repeats = _look_for_repeats(decompressed_data, current_byte);
        decompressed_data.nrl_advance(repeats as usize);
        if current_byte == 0 {
            // CMD_ZERO_OUT
            debug_assert!(repeats < CMD_ZERO_OUT);
            compressed_data.put_u8(repeats); // cmd byte
        } else {
            // CMD_FILL_OUT
            // Too big for one cmd, just make it into two.
            if repeats > NRL_LOOKAHEAD_FILL_MAX_BYTES {
                let repeats_byte1 = repeats - NRL_LOOKAHEAD_FILL_MAX_BYTES;
                // -1 because each cmd byte in itself codes 1 output
                // 2 + 63 + 63 = 127 + 1 = 128
                let cmd_byte1 = CMD_FILL_OUT + (repeats_byte1 - 1);
                let cmd_byte2 = CMD_FILL_OUT + (repeats - repeats_byte1);
                compressed_data.put_u8(cmd_byte1); // cmd byte
                compressed_data.put_u8(current_byte);
                compressed_data.put_u8(cmd_byte2); // cmd byte
                compressed_data.put_u8(current_byte);
            } else {
                compressed_data.put_u8(CMD_FILL_OUT + repeats); // cmd byte
                compressed_data.put_u8(current_byte);
            }
        }
    }
}

/// Look how often the byte in the input data repeats, up to NRL_LOOKAHEAD_ZERO_MAX_BYTES.
fn _look_for_repeats<T, U>(decompressed_data: &T, needle: u8) -> u8
where
    T: NrlRead<U> + Clone,
    U: AsRef<[u8]>
{
    // we really want to make sure the trait impl Clone of T is used and no auto deref happens.
    let mut nc = Clone::clone(decompressed_data);
    let mut repeats = 0;
    while nc.has_remaining() && nc.nrl_get() == needle && repeats < NRL_LOOKAHEAD_ZERO_MAX_BYTES {
        repeats += 1;
    }
    repeats
}

/// Look ahead for the next byte sequence until the first repeating pattern starts.
fn _look_ahead_byte_sequence<T, U>(decompressed_data: &T) -> Bytes
where
    T: NrlRead<U> + Clone,
    U: AsRef<[u8]>
{
    let mut seq = BytesMut::with_capacity(NRL_LOOKAHEAD_COPY_BYTES_MAX_BYTES as usize);
    // If the repeat counter reaches NRL_MIN_SEQ_LEN, the sequence ends NRL_MIN_SEQ_LEN entries before that
    let mut repeat_counter = 0;
    let mut previous_byt_at_pos = None;
    // we really want to make sure the trait impl Clone of T is used and no auto deref happens.
    let mut nc = Clone::clone(decompressed_data);
    loop {
        let byt_at_pos = nc.nrl_get();
        repeat_counter = if Some(byt_at_pos) == previous_byt_at_pos {
            repeat_counter + 1
        } else {
            0
        };

        previous_byt_at_pos = Some(byt_at_pos);
        seq.put_u8(byt_at_pos);

        if repeat_counter > NRL_MIN_SEQ_LEN {
            seq.truncate(seq.len() - NRL_MIN_SEQ_LEN - 1);
            break;
        }

        if seq.len() + 1 >= NRL_LOOKAHEAD_COPY_BYTES_MAX_BYTES as usize || !nc.has_remaining() {
            break;
        }

    }
    seq.freeze()
}

pub(crate) fn decompression_step<T, W>(
    compressed_data: &mut Cursor<T>, decompressed_data: &mut W
) where
    T: AsRef<[u8]>,
    W: NrlWrite
{
    let cmd = compressed_data.get_u8();
    if cmd < CMD_ZERO_OUT {
        // cmd encodes how many 0s to write
        for _ in 0..cmd+1 {
            decompressed_data.nrl_put(0);
        }
    } else if CMD_FILL_OUT <= cmd && cmd < CMD_COPY_BYTES {
        // cmd - CMD_FILL_OUT is the nb of bytes to write
        let param = compressed_data.get_u8();
        for _ in CMD_FILL_OUT-1..cmd {
            decompressed_data.nrl_put(param);
        }
    } else {
        // cmd - CMD_COPY_BYTES is the nb of bytes to write with the sequence of bytes
        for _ in CMD_COPY_BYTES-1..cmd {
            let param = compressed_data.get_u8();
            decompressed_data.nrl_put(param);
        }
    }
}

// "Private" container for compressed data for use with tests written in Python (skytemple-files):
use crate::python::*;

#[pyclass(module = "skytemple_rust._st_generic_nrl_compression")]
#[derive(Clone)]
pub(crate) struct GenericNrlCompressionContainer {
    compressed_data: Bytes,
    length_decompressed: u16
}

impl GenericNrlCompressionContainer {
    pub fn compress(data: &[u8]) -> PyResult<Self> {
        let mut compressed_data = BytesMut::with_capacity(data.len() * 2);
        let mut cursor = Cursor::new(data);
        while NrlRead::has_remaining(&cursor) {
            compression_step(&mut cursor, &mut compressed_data);
        }
        Ok(Self {
            length_decompressed: data.len() as u16, compressed_data: compressed_data.freeze()
        })
    }
    pub fn matches(data: &[u8]) -> bool {
        &data[0..6] == Self::MAGIC
    }
    fn cont_size(mut data: Bytes, byte_offset: usize) -> u16 {
        (data.len() - byte_offset) as u16
    }
}

#[pymethods]
impl GenericNrlCompressionContainer {
    const DATA_START: usize = 8;
    const MAGIC: &'static [u8; 6] = b"GENNRL";

    #[new]
    pub fn new(data: &[u8]) -> PyResult<Self> {
        let mut data = Bytes::from(data.to_vec());
        data.advance(6);
        let length_decompressed = data.get_u16_le();
        Ok(Self {
            compressed_data: data, length_decompressed
        })
    }
    pub fn decompress(&self) -> PyResult<crate::bytes::StBytesMut> {
        let mut compressed_data = Cursor::new(self.compressed_data.clone());
        let mut decompressed_data = BytesMut::with_capacity(self.length_decompressed as usize);

        while decompressed_data.len() < self.length_decompressed as usize {
            if !NrlRead::has_remaining(&compressed_data) {
                return Err(exceptions::PyValueError::new_err(format!(
                    "Generic NRL Decompressor: End result length unexpected. \
                    Should be {}, is {}.",
                    self.length_decompressed, decompressed_data.len()
                )))
            }

            decompression_step(&mut compressed_data, &mut decompressed_data);
        }
        Ok(decompressed_data.into())
    }
    pub fn to_bytes(&self) -> crate::bytes::StBytesMut {
        let mut res = BytesMut::with_capacity(self.compressed_data.len() + Self::DATA_START);
        res.put(Bytes::from_static(Self::MAGIC));
        res.put_u16_le(self.length_decompressed);
        res.put(self.compressed_data.clone());
        res.into()
    }
    #[cfg(feature = "python")]
    #[classmethod]
    #[args(byte_offset = 0)]
    #[pyo3(name = "cont_size")]
    fn _cont_size(_cls: &PyType, data: crate::bytes::StBytes, byte_offset: usize) -> u16 {
        Self::cont_size(data.0, byte_offset)
    }
    #[cfg(feature = "python")]
    #[classmethod]
    #[pyo3(name = "compress")]
    fn _compress(_cls: &PyType, data: &[u8]) -> PyResult<Self> {
        Self::compress(data)
    }
}

#[cfg(feature = "python")]
pub(crate) fn create_st_generic_nrl_compression_module(py: Python) -> PyResult<(&str, &PyModule)> {
    let name: &'static str = "skytemple_rust._st_generic_nrl_compression";
    let m = PyModule::new(py, name)?;
    m.add_class::<GenericNrlCompressionContainer>()?;

    Ok((name, m))
}
