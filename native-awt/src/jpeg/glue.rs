// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `ACC_NATIVE` methods of `com.sun.imageio.plugins.jpeg.JPEGImageWriter`
//! and `JPEGImageReader` (the JDK's `imageioJPEG.c`), over the libjpeg-exact
//! codec in this module. The genuine Java plugin classes run unmodified; these
//! natives do what `libjavajpeg` does: pull bytes from the Java stream through
//! the reader's callbacks, push compressed bytes through the writer's, and move
//! scanlines through the Java-side one-line buffers.

use std::collections::HashMap;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeHandle, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use super::dec::{ByteSource, ColorConvert, Decoder, HeaderOutcome, JpegError};
use super::enc::{CompSpec, EncParams, Encoder};
use super::std_tables::*;
use super::HuffSpec;

const READER: &str = "com/sun/imageio/plugins/jpeg/JPEGImageReader";
const WRITER: &str = "com/sun/imageio/plugins/jpeg/JPEGImageWriter";

struct ReaderState {
    dec: Decoder,
    buf: Vec<u8>,
    pos: usize,
    /// 0 = stream live, 1 = fake EOI's 0xFF emitted, 2 = fake EOI done.
    eof_stage: u8,
    out_cs: Option<i32>,
    abort: bool,
}

#[derive(Default)]
struct WriterState {
    abort: bool,
}

#[derive(Default)]
struct JpegTable {
    next: i64,
    readers: HashMap<i64, ReaderState>,
    writers: HashMap<i64, WriterState>,
}

static JPEGS: VmScoped<JpegTable> = VmScoped::new();

/// Drop one VM's JPEG state.
pub fn forget_vm(vm: usize) {
    JPEGS.forget(vm);
}

fn arg_obj(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_int(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn arg_long(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    }
}

fn internal(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::InternalError {
        message: msg.into(),
    }
    .into()
}

fn throw_iio(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let s = ctx.create_string(msg);
    match ctx.construct_throwable(
        "javax/imageio/IIOException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    ) {
        Ok(Some(obj)) => MethodCallFailed::ExceptionThrown(obj),
        Err(e) => e,
        Ok(None) => RuntimeError::IOException {
            message: msg.to_owned(),
        }
        .into(),
    }
}

fn read_int_array(ctx: &dyn NativeContext, arr: Option<ObjectRef>) -> Vec<i32> {
    match arr {
        Some(a) => {
            let n = ctx.array_length(a);
            let mut v = vec![0i32; n];
            ctx.read_int_array_into(a, 0, &mut v);
            v
        }
        None => Vec::new(),
    }
}

fn read_short_array(ctx: &dyn NativeContext, arr: Option<ObjectRef>) -> Vec<u8> {
    match arr {
        Some(a) => (0..ctx.array_length(a))
            .map(|i| match ctx.get_array_element(a, i) {
                Value::Int(v) => v as u8,
                _ => 0,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn q_table(ctx: &dyn NativeContext, o: ObjectRef) -> Option<[u16; 64]> {
    let a = match ctx.get_field_by_name(o, "qTable") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let v = read_int_array(ctx, Some(a));
    if v.len() < 64 {
        return None;
    }
    let mut t = [0u16; 64];
    for i in 0..64 {
        t[i] = v[i] as u16;
    }
    Some(t)
}

fn huff_table(ctx: &dyn NativeContext, o: ObjectRef) -> Option<HuffSpec> {
    let l = match ctx.get_field_by_name(o, "lengths") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let v = match ctx.get_field_by_name(o, "values") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    Some(HuffSpec::from_lengths(
        &read_short_array(ctx, Some(l)),
        &read_short_array(ctx, Some(v)),
    ))
}

fn objects(ctx: &dyn NativeContext, arr: Option<ObjectRef>) -> Vec<Option<ObjectRef>> {
    match arr {
        Some(a) => (0..ctx.array_length(a))
            .map(|i| match ctx.get_array_element(a, i) {
                Value::Object(o) => o,
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    }
}

fn n_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

fn new_id(ctx: &dyn NativeContext, reader: bool) -> i64 {
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| {
        t.next += 1;
        let id = t.next;
        if reader {
            t.readers.insert(
                id,
                ReaderState {
                    dec: Decoder::new(),
                    buf: Vec::new(),
                    pos: 0,
                    eof_stage: 0,
                    out_cs: None,
                    abort: false,
                },
            );
        } else {
            t.writers.insert(id, WriterState::default());
        }
        id
    })
}

/// `disposeReader` / `disposeWriter` (static): the pointer is the first long.
fn n_dispose(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = args
        .iter()
        .find_map(|v| {
            if let Value::Long(l) = v {
                Some(*l)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| {
        t.readers.remove(&id);
        t.writers.remove(&id);
    });
    Ok(None)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

fn jw_init(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(new_id(ctx, false))))
}

fn jw_set_abort(ctx: &mut dyn NativeContext, args: &[Value], on: bool) {
    let id = arg_long(args, 1);
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| {
        if let Some(w) = t.writers.get_mut(&id) {
            w.abort = on;
        }
    });
}

fn jw_abort(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jw_set_abort(ctx, args, true);
    Ok(None)
}

fn jw_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jw_set_abort(ctx, args, false);
    Ok(None)
}

fn writer_aborted(ctx: &dyn NativeContext, id: i64) -> bool {
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| t.writers.get(&id).map(|w| w.abort).unwrap_or(false))
}

/// Push bytes to `JPEGImageWriter.writeOutputData`.
fn emit(
    scope: &mut NativeHandleScope<'_>,
    this_h: &NativeHandle,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    const CHUNK: usize = 1 << 16;
    for chunk in bytes.chunks(CHUNK) {
        let arr = scope.new_array(ArrayElementType::Byte, chunk.len());
        let arr_h = scope.root(arr);
        let arr = scope.get(&arr_h);
        scope.write_byte_array_from(arr, 0, chunk);
        let this = scope.get(this_h);
        let arr = scope.get(&arr_h);
        scope.invoke_special(
            WRITER,
            "writeOutputData",
            "([BII)V",
            &[
                Value::Object(Some(this)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(chunk.len() as i32),
            ],
        )?;
    }
    Ok(())
}

/// `jpeg_set_colorspace` defaults for component `i`: `(tq, td, ta)`.
fn default_comp_tables(out_cs: i32, i: usize) -> (usize, usize, usize) {
    match out_cs {
        3 | 5 | 7 | 10 => {
            if i == 0 {
                (0, 0, 0)
            } else {
                (1, 1, 1)
            }
        }
        11 => {
            if i == 1 || i == 2 {
                (1, 1, 1)
            } else {
                (0, 0, 0)
            }
        }
        _ => (0, 0, 0),
    }
}

/// `JPEGImageWriter.writeImage(...)`.
fn jw_write_image(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("writeImage: null receiver"))?;
    let sp = arg_long(args, 1);
    let data = arg_obj(args, 2).ok_or_else(|| internal("writeImage: null buffer"))?;
    let in_cs = arg_int(args, 3);
    let out_cs = arg_int(args, 4);
    let num_bands = arg_int(args, 5).max(1) as usize;
    let band_sizes = read_int_array(ctx, arg_obj(args, 6));
    let src_width = arg_int(args, 7).max(0) as usize;
    let dest_width = arg_int(args, 8).max(0) as usize;
    let dest_height = arg_int(args, 9).max(0) as usize;
    let step_x = arg_int(args, 10).max(1) as usize;
    let step_y = arg_int(args, 11).max(1) as usize;
    let qt_objs = objects(ctx, arg_obj(args, 12));
    let write_dqt = arg_int(args, 13) != 0;
    let dc_objs = objects(ctx, arg_obj(args, 14));
    let ac_objs = objects(ctx, arg_obj(args, 15));
    let write_dht = arg_int(args, 16) != 0;
    let optimize = arg_int(args, 17) != 0;
    let ids = read_int_array(ctx, arg_obj(args, 21));
    let hs = read_int_array(ctx, arg_obj(args, 22));
    let vs = read_int_array(ctx, arg_obj(args, 23));
    let qs = read_int_array(ctx, arg_obj(args, 24));
    let have_metadata = arg_int(args, 25) != 0;
    let restart = arg_int(args, 26).max(0) as usize;

    if dest_width == 0 || dest_height == 0 {
        return Err(throw_iio(ctx, "Empty JPEG image (DNL not supported)"));
    }

    let mut qtables: [Option<[u16; 64]>; 4] = [None; 4];
    for (i, o) in qt_objs.iter().enumerate().take(4) {
        if let Some(o) = o {
            qtables[i] = q_table(ctx, *o);
        }
    }
    let mut dc_tables: [Option<HuffSpec>; 4] = [None, None, None, None];
    let mut ac_tables: [Option<HuffSpec>; 4] = [None, None, None, None];
    for (i, o) in dc_objs.iter().enumerate().take(4) {
        if let Some(o) = o {
            dc_tables[i] = huff_table(ctx, *o);
        }
    }
    for (i, o) in ac_objs.iter().enumerate().take(4) {
        if let Some(o) = o {
            ac_tables[i] = huff_table(ctx, *o);
        }
    }
    // Optimised streams pass no tables: use the standard ones.
    if dc_tables.iter().all(|t| t.is_none()) {
        dc_tables[0] = Some(HuffSpec::from_lengths(
            STDDCLUMINANCE_LENGTHS,
            STDDCLUMINANCE_VALUES,
        ));
        dc_tables[1] = Some(HuffSpec::from_lengths(
            STDDCCHROMINANCE_LENGTHS,
            STDDCCHROMINANCE_VALUES,
        ));
    }
    if ac_tables.iter().all(|t| t.is_none()) {
        ac_tables[0] = Some(HuffSpec::from_lengths(
            STDACLUMINANCE_LENGTHS,
            STDACLUMINANCE_VALUES,
        ));
        ac_tables[1] = Some(HuffSpec::from_lengths(
            STDACCHROMINANCE_LENGTHS,
            STDACCHROMINANCE_VALUES,
        ));
    }
    if qtables.iter().all(|t| t.is_none()) {
        qtables[0] = Some(K1);
        qtables[1] = Some(K2);
    }
    let first_q = qtables.iter().flatten().next().copied().unwrap_or(K1);
    let first_dc = dc_tables
        .iter()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_else(|| HuffSpec::from_lengths(STDDCLUMINANCE_LENGTHS, STDDCLUMINANCE_VALUES));
    let first_ac = ac_tables
        .iter()
        .flatten()
        .next()
        .cloned()
        .unwrap_or_else(|| HuffSpec::from_lengths(STDACLUMINANCE_LENGTHS, STDACLUMINANCE_VALUES));

    let mut comps = Vec::new();
    for i in 0..num_bands {
        let (tq, td, ta) = default_comp_tables(out_cs, i);
        let default_id = match out_cs {
            2 => [82u8, 71, 66, 65][i.min(3)],
            _ => (i + 1) as u8,
        };
        comps.push(CompSpec {
            id: ids.get(i).map(|&v| v as u8).unwrap_or(default_id),
            h: hs.get(i).map(|&v| v.max(1) as usize).unwrap_or(1),
            v: vs.get(i).map(|&v| v.max(1) as usize).unwrap_or(1),
            tq: qs.get(i).map(|&v| v.max(0) as usize).unwrap_or(tq).min(3),
            td,
            ta,
        });
    }
    for c in &comps {
        if qtables[c.tq].is_none() {
            qtables[c.tq] = Some(first_q);
        }
        if dc_tables[c.td].is_none() {
            dc_tables[c.td] = Some(first_dc.clone());
        }
        if ac_tables[c.ta].is_none() {
            ac_tables[c.ta] = Some(first_ac.clone());
        }
    }
    let params = EncParams {
        width: dest_width,
        height: dest_height,
        in_comps: num_bands,
        rgb_to_ycc: in_cs == 2 && out_cs == 3 && num_bands == 3,
        comps,
        qtables,
        dc_tables,
        ac_tables,
        write_dqt,
        write_dht: write_dht || optimize,
        restart_interval: restart,
        optimize,
    };
    let enc = Encoder::new(params);

    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let data_h = scope.root(data);

    // `jpeg_start_compress` writes SOI; the metadata callback follows.
    emit(&mut scope, &this_h, &enc.soi())?;
    if have_metadata {
        let t = scope.get(&this_h);
        scope.invoke_special(WRITER, "writeMetadata", "()V", &[Value::Object(Some(t))])?;
    }

    // Scanlines: `grabPixels(y)` fills the one-line buffer, which we read.
    let scale: Vec<Option<Vec<u8>>> = (0..num_bands)
        .map(|b| {
            let bs = band_sizes.get(b).copied().unwrap_or(8);
            if bs == 8 || !(1..8).contains(&bs) {
                None
            } else {
                let maxv = (1u32 << bs) - 1;
                Some(
                    (0..=maxv)
                        .map(|j| ((j * 255 + maxv / 2) / maxv) as u8)
                        .collect(),
                )
            }
        })
        .collect();
    let mut rows: Vec<Vec<u8>> = Vec::with_capacity(dest_height);
    let mut line = vec![0u8; src_width * num_bands];
    let mut ty = 0usize;
    for _ in 0..dest_height {
        if writer_aborted(&*scope, sp) {
            return Ok(Some(Value::Int(1)));
        }
        let t = scope.get(&this_h);
        scope.invoke_special(
            WRITER,
            "grabPixels",
            "(I)V",
            &[Value::Object(Some(t)), Value::Int(ty as i32)],
        )?;
        let d = scope.get(&data_h);
        scope.read_byte_array_into(d, 0, &mut line);
        let mut row = Vec::with_capacity(dest_width * num_bands);
        for px in 0..dest_width {
            let x = (px * step_x).min(src_width.saturating_sub(1));
            for b in 0..num_bands {
                let v = line[x * num_bands + b];
                row.push(match &scale[b] {
                    Some(tab) => tab.get(v as usize).copied().unwrap_or(v),
                    None => v,
                });
            }
        }
        rows.push(row);
        ty += step_y;
    }
    let (scan_header, data) = enc.encode(&rows);
    let mut out = enc.frame_header();
    out.extend(scan_header);
    out.extend(data);
    out.extend(enc.eoi());
    emit(&mut scope, &this_h, &out)?;
    Ok(Some(Value::Int(0)))
}

/// `JPEGImageWriter.writeTables(long, JPEGQTable[], JPEGHuffmanTable[], JPEGHuffmanTable[])`.
fn jw_write_tables(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("writeTables: null receiver"))?;
    let qt = objects(ctx, arg_obj(args, 2));
    let dc = objects(ctx, arg_obj(args, 3));
    let ac = objects(ctx, arg_obj(args, 4));
    let mut out = vec![0xff, 0xd8];
    for (i, o) in qt.iter().enumerate().take(4) {
        if let Some(t) = o.and_then(|o| q_table(ctx, o)) {
            let prec = t.iter().any(|&v| v > 255);
            out.extend_from_slice(&[0xff, 0xdb]);
            let len: usize = if prec { 64 * 2 + 1 + 2 } else { 64 + 1 + 2 };
            out.extend_from_slice(&(len as u16).to_be_bytes());
            out.push(i as u8 + if prec { 16 } else { 0 });
            for k in 0..64 {
                let v = t[super::NATURAL_ORDER[k]];
                if prec {
                    out.push((v >> 8) as u8);
                }
                out.push(v as u8);
            }
        }
    }
    for (is_ac, set) in [(false, &dc), (true, &ac)] {
        for (i, o) in set.iter().enumerate().take(4) {
            if let Some(h) = o.and_then(|o| huff_table(ctx, o)) {
                out.extend_from_slice(&[0xff, 0xc4]);
                let len = 2 + 1 + 16 + h.vals.len();
                out.extend_from_slice(&(len as u16).to_be_bytes());
                out.push(i as u8 + if is_ac { 0x10 } else { 0 });
                for l in 1..=16 {
                    out.push(h.bits[l]);
                }
                out.extend_from_slice(&h.vals);
            }
        }
    }
    out.extend_from_slice(&[0xff, 0xd9]);
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    emit(&mut scope, &this_h, &out)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

fn jr_init(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(new_id(ctx, true))))
}

fn with_reader<R>(
    ctx: &dyn NativeContext,
    id: i64,
    f: impl FnOnce(&mut ReaderState) -> R,
) -> Option<R> {
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| t.readers.get_mut(&id).map(f))
}

fn jr_set_source(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 1);
    with_reader(ctx, id, |r| {
        r.buf.clear();
        r.pos = 0;
        r.eof_stage = 0;
        r.abort = false;
    });
    Ok(None)
}

fn jr_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 1);
    with_reader(ctx, id, |r| {
        r.dec = Decoder::new();
        r.buf.clear();
        r.pos = 0;
        r.eof_stage = 0;
        r.out_cs = None;
        r.abort = false;
    });
    Ok(None)
}

fn jr_abort(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 1);
    with_reader(ctx, id, |r| r.abort = true);
    Ok(None)
}

fn jr_clear_abort(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 1);
    with_reader(ctx, id, |r| r.abort = false);
    Ok(None)
}

fn jr_set_out_cs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 1);
    let cs = arg_int(args, 2);
    with_reader(ctx, id, |r| r.out_cs = Some(cs));
    Ok(None)
}

fn take_reader(ctx: &dyn NativeContext, id: i64) -> Option<ReaderState> {
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| t.readers.remove(&id))
}

fn put_reader(ctx: &dyn NativeContext, id: i64, st: ReaderState) {
    let vm = ctx.vm_identity();
    JPEGS.with(vm, |t| {
        t.readers.insert(id, st);
    });
}

struct JavaSrc<'s, 'c> {
    scope: &'s mut NativeHandleScope<'c>,
    this_h: NativeHandle,
    arr_h: NativeHandle,
    buf: &'s mut Vec<u8>,
    pos: &'s mut usize,
    stage: &'s mut u8,
    err: Option<MethodCallFailed>,
}

impl ByteSource for JavaSrc<'_, '_> {
    fn next_byte(&mut self) -> Option<u8> {
        if *self.pos < self.buf.len() {
            let b = self.buf[*self.pos];
            *self.pos += 1;
            return Some(b);
        }
        if self.err.is_some() {
            return None;
        }
        match *self.stage {
            1 => {
                *self.stage = 2;
                return Some(0xd9);
            }
            2 => return None,
            _ => {}
        }
        let this = self.scope.get(&self.this_h);
        let arr = self.scope.get(&self.arr_h);
        let cap = self.scope.array_length(arr) as i32;
        match self.scope.invoke_special(
            READER,
            "readInputData",
            "([BII)I",
            &[
                Value::Object(Some(this)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(cap),
            ],
        ) {
            Ok(Some(Value::Int(n))) if n > 0 => {
                let arr = self.scope.get(&self.arr_h);
                self.buf.clear();
                self.buf.resize(n as usize, 0);
                self.scope.read_byte_array_into(arr, 0, self.buf);
                *self.pos = 1;
                Some(self.buf[0])
            }
            Ok(_) => {
                // End of stream: libjpeg inserts a fake EOI marker.
                *self.stage = 1;
                self.buf.clear();
                *self.pos = 0;
                Some(0xff)
            }
            Err(e) => {
                self.err = Some(e);
                None
            }
        }
    }
}

fn default_out_cs(jcs: i32) -> i32 {
    match jcs {
        1 => 1,
        2 | 3 => 2,
        4 | 11 => 4,
        _ => jcs,
    }
}

fn jpeg_msg(e: &JpegError) -> String {
    match e {
        JpegError::Msg(m) => m.clone(),
        JpegError::Truncated => "Premature end of JPEG file".to_owned(),
    }
}

/// `JPEGImageReader.readImageHeader(long, boolean, boolean)`.
fn jr_read_header(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("readImageHeader: null receiver"))?;
    let id = arg_long(args, 1);
    let clear = arg_int(args, 2) != 0;
    let Some(mut st) = take_reader(ctx, id) else {
        return Err(internal("JPEG reader state missing"));
    };
    if clear {
        st.buf.clear();
        st.pos = 0;
        st.eof_stage = 0;
    }
    let outcome;
    let mut java_err = None;
    {
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let arr = scope.new_array(ArrayElementType::Byte, 4096);
        let arr_h = scope.root(arr);
        let ReaderState {
            dec,
            buf,
            pos,
            eof_stage,
            ..
        } = &mut st;
        let mut src = JavaSrc {
            scope: &mut scope,
            this_h,
            arr_h,
            buf,
            pos,
            stage: eof_stage,
            err: None,
        };
        outcome = dec.read_header(&mut src);
        if let Some(e) = src.err.take() {
            java_err = Some(e);
        }
    }
    if let Some(e) = java_err {
        put_reader(ctx, id, st);
        return Err(e);
    }
    match outcome {
        Err(e) => {
            let msg = jpeg_msg(&e);
            st.dec = Decoder::new();
            put_reader(ctx, id, st);
            Err(throw_iio(ctx, &msg))
        }
        Ok(HeaderOutcome::TablesOnly) => {
            put_reader(ctx, id, st);
            Ok(Some(Value::Int(1)))
        }
        Ok(HeaderOutcome::Image) => {
            let jcs = st.dec.jpeg_color_space();
            let out_cs = st.out_cs.unwrap_or_else(|| default_out_cs(jcs));
            let (w, h, n) = (
                st.dec.width as i32,
                st.dec.height as i32,
                st.dec.num_components() as i32,
            );
            let icc = st.dec.icc.clone();
            put_reader(ctx, id, st);
            let mut scope = NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let icc_h = match icc {
                Some(bytes) => {
                    let a = scope.new_array(ArrayElementType::Byte, bytes.len());
                    let h = scope.root(a);
                    let a = scope.get(&h);
                    scope.write_byte_array_from(a, 0, &bytes);
                    Some(h)
                }
                None => None,
            };
            let t = scope.get(&this_h);
            let icc_v = match &icc_h {
                Some(h) => Value::Object(Some(scope.get(h))),
                None => Value::Object(None),
            };
            scope.invoke_special(
                READER,
                "setImageData",
                "(IIIII[B)V",
                &[
                    Value::Object(Some(t)),
                    Value::Int(w),
                    Value::Int(h),
                    Value::Int(jcs),
                    Value::Int(out_cs),
                    Value::Int(n),
                    icc_v,
                ],
            )?;
            Ok(Some(Value::Int(0)))
        }
    }
}

/// `JPEGImageReader.readImage(...)`.
fn jr_read_image(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("readImage: null receiver"))?;
    let id = arg_long(args, 2);
    let buffer = arg_obj(args, 3).ok_or_else(|| internal("readImage: null buffer"))?;
    let num_rbands = arg_int(args, 4).max(1) as usize;
    let src_bands = read_int_array(ctx, arg_obj(args, 5));
    let band_sizes = read_int_array(ctx, arg_obj(args, 6));
    let sx0 = arg_int(args, 7).max(0) as usize;
    let sy0 = arg_int(args, 8).max(0) as usize;
    let swidth = arg_int(args, 9).max(0) as usize;
    let sheight = arg_int(args, 10).max(0) as usize;
    let step_x = arg_int(args, 11).max(1) as usize;
    let step_y = arg_int(args, 12).max(1) as usize;
    let q_objs = objects(ctx, arg_obj(args, 13));
    let dc_objs = objects(ctx, arg_obj(args, 14));
    let ac_objs = objects(ctx, arg_obj(args, 15));

    let Some(mut st) = take_reader(ctx, id) else {
        return Err(internal("JPEG reader state missing"));
    };
    if st.abort {
        st.abort = false;
        put_reader(ctx, id, st);
        return Ok(Some(Value::Int(1)));
    }
    // Abbreviated-stream tables from the read param.
    if q_objs
        .iter()
        .chain(dc_objs.iter())
        .chain(ac_objs.iter())
        .any(|o| o.is_some())
    {
        let mut q: [Option<[u16; 64]>; 4] = [None; 4];
        let mut dc: [Option<HuffSpec>; 4] = [None, None, None, None];
        let mut ac: [Option<HuffSpec>; 4] = [None, None, None, None];
        for (i, o) in q_objs.iter().enumerate().take(4) {
            if let Some(o) = o {
                q[i] = q_table(ctx, *o);
            }
        }
        for (i, o) in dc_objs.iter().enumerate().take(4) {
            if let Some(o) = o {
                dc[i] = huff_table(ctx, *o);
            }
        }
        for (i, o) in ac_objs.iter().enumerate().take(4) {
            if let Some(o) = o {
                ac[i] = huff_table(ctx, *o);
            }
        }
        st.dec.set_tables(q, dc, ac);
    }

    let result;
    let mut java_err = None;
    {
        let mut scope = NativeHandleScope::new(ctx);
        let this_h = scope.root(this);
        let arr = scope.new_array(ArrayElementType::Byte, 4096);
        let arr_h = scope.root(arr);
        let ReaderState {
            dec,
            buf,
            pos,
            eof_stage,
            ..
        } = &mut st;
        let mut src = JavaSrc {
            scope: &mut scope,
            this_h,
            arr_h,
            buf,
            pos,
            stage: eof_stage,
            err: None,
        };
        result = dec.decode_scans(&mut src);
        if let Some(e) = src.err.take() {
            java_err = Some(e);
        }
    }
    if let Some(e) = java_err {
        put_reader(ctx, id, st);
        return Err(e);
    }
    if let Err(e) = result {
        let msg = jpeg_msg(&e);
        st.dec = Decoder::new();
        put_reader(ctx, id, st);
        return Err(throw_iio(ctx, &msg));
    }

    let jcs = st.dec.jpeg_color_space();
    let out_cs = st.out_cs.unwrap_or_else(|| default_out_cs(jcs));
    let convert = match (jcs, out_cs) {
        (3, 2) => ColorConvert::YccToRgb,
        (11, 4) => ColorConvert::YcckToCmyk,
        _ => ColorConvert::None,
    };
    let rows = st.dec.reconstruct(convert, true);
    let ncomp = st.dec.num_components();
    let (img_w, img_h) = (st.dec.width, st.dec.height);
    let unused = st.buf.len().saturating_sub(st.pos);
    st.buf.clear();
    st.pos = 0;
    st.eof_stage = 0;
    put_reader(ctx, id, st);

    // Deliver the region of interest scanline by scanline.
    let dest_w = swidth.div_ceil(step_x);
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let buf_h = scope.root(buffer);
    let scales: Vec<Option<Vec<u8>>> = (0..num_rbands)
        .map(|b| {
            let bs = band_sizes.get(b).copied().unwrap_or(8);
            if bs == 8 || !(1..8).contains(&bs) {
                None
            } else {
                let maxv = (1u32 << bs) - 1;
                Some(
                    (0..256u32)
                        .map(|v| ((v * maxv + 127) / 255) as u8)
                        .collect(),
                )
            }
        })
        .collect();
    let mut out_row = vec![0u8; dest_w * num_rbands];
    let mut target_row = 0usize;
    let mut y = sy0;
    while y < sy0 + sheight && y < img_h {
        let row = &rows[y];
        for px in 0..dest_w {
            let x = (sx0 + px * step_x).min(img_w.saturating_sub(1));
            for b in 0..num_rbands {
                let band = src_bands.get(b).copied().unwrap_or(b as i32).max(0) as usize;
                let v = row[x * ncomp + band.min(ncomp - 1)];
                out_row[px * num_rbands + b] = match &scales[b] {
                    Some(t) => t[v as usize],
                    None => v,
                };
            }
        }
        let bb = scope.get(&buf_h);
        scope.write_byte_array_from(bb, 0, &out_row);
        let t = scope.get(&this_h);
        scope.invoke_special(
            READER,
            "acceptPixels",
            "(IZ)V",
            &[
                Value::Object(Some(t)),
                Value::Int(target_row as i32),
                Value::Int(0),
            ],
        )?;
        target_row += 1;
        y += step_y;
    }
    // Hand back the read-ahead bytes so the next image can be located.
    if unused > 0 {
        let t = scope.get(&this_h);
        scope.invoke_special(
            READER,
            "pushBack",
            "(I)V",
            &[Value::Object(Some(t)), Value::Int(unused as i32)],
        )?;
    }
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the JPEG plugin natives (genuine `ACC_NATIVE` methods: `Bridge`).
pub fn register(registry: &mut NativeMethodRegistry) {
    registry.register(
        READER,
        "initReaderIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V",
        n_noop,
    );
    registry.register(
        WRITER,
        "initWriterIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        n_noop,
    );
    registry.register(READER, "initJPEGImageReader", "()J", jr_init);
    registry.register(READER, "setSource", "(J)V", jr_set_source);
    registry.register(READER, "readImageHeader", "(JZZ)Z", jr_read_header);
    registry.register(READER, "setOutColorSpace", "(JI)V", jr_set_out_cs);
    registry.register(
        READER,
        "readImage",
        "(IJ[BI[I[IIIIIII[Ljavax/imageio/plugins/jpeg/JPEGQTable;[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;IIZ)Z",
        jr_read_image,
    );
    registry.register(READER, "clearNativeReadAbortFlag", "(J)V", jr_clear_abort);
    registry.register(READER, "abortRead", "(J)V", jr_abort);
    registry.register(READER, "resetLibraryState", "(J)V", jr_reset);
    registry.register(READER, "resetReader", "(J)V", jr_reset);
    registry.register(READER, "disposeReader", "(J)V", n_dispose);

    registry.register(WRITER, "initJPEGImageWriter", "()J", jw_init);
    registry.register(WRITER, "setDest", "(J)V", n_noop);
    registry.register(
        WRITER,
        "writeImage",
        "(J[BIII[IIIIII[Ljavax/imageio/plugins/jpeg/JPEGQTable;Z[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;ZZZI[I[I[I[I[IZI)Z",
        jw_write_image,
    );
    registry.register(
        WRITER,
        "writeTables",
        "(J[Ljavax/imageio/plugins/jpeg/JPEGQTable;[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;[Ljavax/imageio/plugins/jpeg/JPEGHuffmanTable;)V",
        jw_write_tables,
    );
    registry.register(WRITER, "abortWrite", "(J)V", jw_abort);
    registry.register(WRITER, "resetWriter", "(J)V", jw_reset);
    registry.register(WRITER, "disposeWriter", "(J)V", n_dispose);
}
