//!
//! Rust Firebird Client
//!
//! Representation of a fetched row
//!

use rsfbclient_core::{Charset, Column, FbError, SqlType};
use std::{mem, result::Result};

use crate::{ibase, ibase::IBase, status::Status, varchar::Varchar};

use ColumnBufferData::*;

#[derive(Debug)]
/// Types supported by the crate
pub enum ColumnBufferData {
    /// Coerces to Varchar
    Text(Varchar),
    /// Coerces to Int64
    Integer(Box<i64>),
    /// Coerces to Double
    Float(Box<f64>),
    /// Coerces to Timestamp
    Timestamp(Box<ibase::ISC_TIMESTAMP>),
    /// Coerces to Blob sub_type 1
    BlobText(Box<ibase::GDS_QUAD_t>),
    /// Coerces to Blob sub_type 0
    BlobBinary(Box<ibase::GDS_QUAD_t>),
    /// Coerces to boolean. Fb >= 3
    Boolean(Box<i8>),
    /// Multi-dimensional Firebird ARRAY (SQL_ARRAY, type code 540).
    /// The engine fills the `id` quad with the array reference; the
    /// relation + column names are captured at describe-time so
    /// `isc_array_lookup_bounds` can resolve the element layout.
    Array {
        id: Box<ibase::GDS_QUAD_t>,
        relation: String,
        column: String,
    },
    /// `INT128` (Firebird 4+, SQL type code 32752). 16 bytes, signed,
    /// little-endian two's complement. Decoded as `i128`.
    Int128(Box<[u8; 16]>),
    /// `TIME WITH TIME ZONE` (SQL type code 32756). 8 bytes:
    /// 4-byte ISC_TIME + 2-byte timezone id + 2-byte padding.
    TimeTz(Box<[u8; 8]>),
    /// `TIMESTAMP WITH TIME ZONE` (SQL type code 32754). 12 bytes:
    /// 8-byte ISC_TIMESTAMP + 2-byte timezone id + 2-byte padding.
    TimestampTz(Box<[u8; 12]>),
    /// `DECFLOAT(16)` (SQL type code 32760). 8 bytes, IEEE 754-2008
    /// decimal64 (DPD coefficient). Surfaced as a hex string —
    /// real decimal-floating decode would require linking against
    /// fbclient's `fb_dec16_*` helpers or a DPD implementation.
    Dec16(Box<[u8; 8]>),
    /// `DECFLOAT(34)` (SQL type code 32762). 16 bytes, IEEE 754-2008
    /// decimal128. Same caveat as [`Dec16`](Self::Dec16).
    Dec34(Box<[u8; 16]>),
}

impl ColumnBufferData {
    fn as_mut_ptr(&mut self) -> *mut ibase::ISC_SCHAR {
        match self {
            Text(v) => v.as_ptr() as _,
            Integer(i) => &**i as *const _ as _,
            Float(f) => &**f as *const _ as _,
            Timestamp(ts) => &**ts as *const _ as _,
            BlobText(bid) => &**bid as *const _ as _,
            BlobBinary(bid) => &**bid as *const _ as _,
            Boolean(b) => &**b as *const _ as _,
            Array { id, .. } => &**id as *const _ as _,
            Int128(b) => b.as_mut_ptr() as _,
            TimeTz(b) => b.as_mut_ptr() as _,
            TimestampTz(b) => b.as_mut_ptr() as _,
            Dec16(b) => b.as_mut_ptr() as _,
            Dec34(b) => b.as_mut_ptr() as _,
        }
    }
}

#[derive(Debug)]
/// Allocates memory for a column
pub struct ColumnBuffer {
    /// Buffer for the column data
    buffer: ColumnBufferData,

    /// Null indicator
    nullind: Box<i16>,

    /// Column name
    col_name: String,

    raw_type: i16,
}

impl ColumnBuffer {
    /// Allocate a buffer from an output (column) XSQLVAR, coercing the data types as necessary
    pub fn from_xsqlvar(var: &mut ibase::XSQLVAR) -> Result<Self, FbError> {
        // Remove nullable type indicator
        let sqltype = var.sqltype & (!1);
        let sqlsubtype = var.sqlsubtype;

        let mut nullind = Box::new(0);
        var.sqlind = &mut *nullind;

        let mut buffer = match sqltype as u32 {
            ibase::SQL_BOOLEAN => {
                var.sqllen = mem::size_of::<i8>() as i16;

                var.sqltype = ibase::SQL_BOOLEAN as i16 + 1;

                Boolean(Box::new(0))
            }

            ibase::SQL_BLOB if (sqlsubtype <= 1) => {
                let blob_id = Box::new(ibase::GDS_QUAD_t {
                    gds_quad_high: 0,
                    gds_quad_low: 0,
                });

                var.sqltype = ibase::SQL_BLOB as i16 + 1;

                // subtype 0: binary
                // subtype <= -1: custom
                if sqlsubtype <= 0 {
                    BlobBinary(blob_id)
                } else {
                    BlobText(blob_id)
                }
            }

            ibase::SQL_TEXT | ibase::SQL_VARYING => {
                var.sqltype = ibase::SQL_VARYING as i16 + 1;

                Text(Varchar::new(var.sqllen as u16))
            }

            ibase::SQL_SHORT | ibase::SQL_LONG | ibase::SQL_INT64 => {
                var.sqllen = mem::size_of::<i64>() as i16;

                if var.sqlscale == 0 {
                    var.sqltype = ibase::SQL_INT64 as i16 + 1;

                    Integer(Box::new(0))
                } else {
                    var.sqlscale = 0;
                    var.sqltype = ibase::SQL_DOUBLE as i16 + 1;

                    Float(Box::new(0.0))
                }
            }

            ibase::SQL_FLOAT | ibase::SQL_DOUBLE => {
                var.sqllen = mem::size_of::<i64>() as i16;

                var.sqltype = ibase::SQL_DOUBLE as i16 + 1;

                Float(Box::new(0.0))
            }

            ibase::SQL_TIMESTAMP | ibase::SQL_TYPE_DATE | ibase::SQL_TYPE_TIME => {
                var.sqllen = mem::size_of::<ibase::ISC_TIMESTAMP>() as i16;

                var.sqltype = ibase::SQL_TIMESTAMP as i16 + 1;

                Timestamp(Box::new(ibase::ISC_TIMESTAMP {
                    timestamp_date: 0,
                    timestamp_time: 0,
                }))
            }

            // Firebird 4+ INT128 (SQL_INT128 = 32752). 16-byte
            // little-endian signed two's complement. Decoded as
            // `i128` at row time.
            32752 => {
                var.sqltype = 32752 + 1;
                var.sqllen = 16;
                Int128(Box::new([0u8; 16]))
            }

            // Firebird 4+ TIMESTAMP WITH TIME ZONE (SQL_TIMESTAMP_TZ
            // = 32754). 12 bytes: `ISC_TIMESTAMP` (8) + `ISC_USHORT`
            // time-zone id (2) + 2 bytes padding.
            32754 => {
                var.sqltype = 32754 + 1;
                var.sqllen = 12;
                TimestampTz(Box::new([0u8; 12]))
            }

            // Firebird 4+ TIME WITH TIME ZONE (SQL_TIME_TZ = 32756).
            // 8 bytes: `ISC_TIME` (4) + `ISC_USHORT` time-zone id (2)
            // + 2 bytes padding.
            32756 => {
                var.sqltype = 32756 + 1;
                var.sqllen = 8;
                TimeTz(Box::new([0u8; 8]))
            }

            // Firebird 4+ DECFLOAT(16) (SQL_DEC16 = 32760). 8-byte
            // IEEE 754-2008 decimal64 (DPD-encoded coefficient).
            32760 => {
                var.sqltype = 32760 + 1;
                var.sqllen = 8;
                Dec16(Box::new([0u8; 8]))
            }

            // Firebird 4+ DECFLOAT(34) (SQL_DEC34 = 32762). 16-byte
            // IEEE 754-2008 decimal128.
            32762 => {
                var.sqltype = 32762 + 1;
                var.sqllen = 16;
                Dec34(Box::new([0u8; 16]))
            }

            // Firebird ARRAY column (multi-dimensional). The engine
            // returns an array reference shaped like `ISC_QUAD`; the
            // body has to be pulled back via `isc_array_get_slice`,
            // which needs the relation + column name to look up the
            // bounds. Both come straight from the XSQLVAR.
            sqltype if sqltype == rsfbclient_core::ibase::SQL_ARRAY => {
                let array_id = Box::new(ibase::GDS_QUAD_t {
                    gds_quad_high: 0,
                    gds_quad_low: 0,
                });

                // Coerce sqltype to the nullable variant. The engine
                // fills `sqldata` with the 8-byte array reference once
                // a row is fetched.
                var.sqltype = (rsfbclient_core::ibase::SQL_ARRAY as i16) + 1;
                var.sqllen = mem::size_of::<ibase::GDS_QUAD_t>() as i16;

                let relation = xsqlvar_relname(var);
                let column = xsqlvar_sqlname(var);

                Array {
                    id: array_id,
                    relation,
                    column,
                }
            }

            sqltype => {
                return Err(format!("Unsupported column type ({} {})", sqltype, sqlsubtype).into())
            }
        };

        var.sqldata = buffer.as_mut_ptr();

        let col_name = {
            let len = usize::min(var.aliasname_length as usize, var.aliasname.len());
            let bname = var.aliasname[..len]
                .iter()
                .map(|b| *b as u8)
                .collect::<Vec<u8>>();

            String::from_utf8(bname)
        }?;

        Ok(ColumnBuffer {
            buffer,
            nullind,
            col_name,
            raw_type: sqltype,
        })
    }

    /// Converts the buffer to a Column
    pub fn to_column<T: IBase>(
        &self,
        db: &mut ibase::isc_db_handle,
        tr: &mut ibase::isc_tr_handle,
        ibase: &T,
        charset: &Charset,
    ) -> Result<Column, FbError> {
        if *self.nullind != 0 {
            return Ok(Column::new(
                self.col_name.clone(),
                self.raw_type as u32,
                SqlType::Null,
            ));
        }

        let col_type = match &self.buffer {
            Text(varchar) => SqlType::Text(charset.decode(varchar.as_bytes())?),

            Integer(i) => SqlType::Integer(**i),

            Float(f) => SqlType::Floating(**f),

            Timestamp(ts) => SqlType::Timestamp(rsfbclient_core::date_time::decode_timestamp(**ts)),

            BlobText(b) => SqlType::Text(blobtext_to_string(**b, db, tr, ibase, charset)?),

            BlobBinary(b) => SqlType::Binary(blobbinary_to_vec(**b, db, tr, ibase)?),

            Boolean(b) => SqlType::Boolean(**b != 0),

            // Render the decoded ARRAY as a SQL_TEXT-shaped JSON
            // literal so callers using the existing `Column::value()`
            // string path see a readable representation. Real-typed
            // surfacing (Vec<i32>, Vec<String>, …) would require
            // extending `rsfbclient-core::SqlType`, which is out of
            // scope for the 0.26 patchset.
            Array {
                id,
                relation,
                column,
            } => SqlType::Text(array_to_string(**id, relation, column, db, tr, ibase, charset)?),

            // Firebird 4+ types decoded into `SqlType::Text` until
            // `rsfbclient-core` gains dedicated variants. The format
            // is deterministic and stable enough for `CAST AS
            // VARCHAR` round-trips and round-the-table comparisons.
            Int128(bytes) => SqlType::Text(int128_to_string(**bytes)),
            TimeTz(bytes) => SqlType::Text(time_tz_to_string(**bytes)),
            TimestampTz(bytes) => SqlType::Text(timestamp_tz_to_string(**bytes)),
            Dec16(bytes) => SqlType::Text(dec16_to_string(**bytes)),
            Dec34(bytes) => SqlType::Text(dec34_to_string(**bytes)),
        };

        Ok(Column::new(
            self.col_name.clone(),
            self.raw_type as u32,
            col_type,
        ))
    }
}

/// Converts a binary blob to a vec<u8>
fn blobbinary_to_vec<T: IBase>(
    blob_id: ibase::GDS_QUAD_t,
    db: &mut ibase::isc_db_handle,
    tr: &mut ibase::isc_tr_handle,
    ibase: &T,
) -> Result<Vec<u8>, FbError> {
    read_blob(blob_id, db, tr, ibase)
}

/// Converts a text blob to a string
fn blobtext_to_string<T: IBase>(
    blob_id: ibase::GDS_QUAD_t,
    db: &mut ibase::isc_db_handle,
    tr: &mut ibase::isc_tr_handle,
    ibase: &T,
    charset: &Charset,
) -> Result<String, FbError> {
    let blob_bytes = read_blob(blob_id, db, tr, ibase)?;

    charset.decode(blob_bytes)
}

/// Read the blob type
fn read_blob<T: IBase>(
    mut blob_id: ibase::GDS_QUAD_t,
    db: &mut ibase::isc_db_handle,
    tr: &mut ibase::isc_tr_handle,
    ibase: &T,
) -> Result<Vec<u8>, FbError> {
    let mut status = Status::default();
    let mut handle = 0;

    let mut blob_bytes = Vec::with_capacity(256);

    unsafe {
        if ibase.isc_open_blob()(&mut status[0], db, tr, &mut handle, &mut blob_id) != 0 {
            return Err(status.as_error(ibase));
        }
    }

    // Assert that the handle is valid
    debug_assert_ne!(handle, 0);

    let mut blob_stat = 0;

    while blob_stat == 0 || status[1] == (ibase::isc_segment as isize) {
        let mut blob_seg_loaded = 0;
        let mut blob_seg_slice = [0_u8; 255];

        blob_stat = unsafe {
            ibase.isc_get_segment()(
                &mut status[0],
                &mut handle,
                &mut blob_seg_loaded,
                blob_seg_slice.len() as u16,
                blob_seg_slice.as_mut_ptr() as *mut std::os::raw::c_char,
            )
        };

        blob_bytes.extend_from_slice(&blob_seg_slice[..blob_seg_loaded as usize]);
    }

    unsafe {
        if ibase.isc_close_blob()(&mut status[0], &mut handle) != 0 {
            return Err(status.as_error(ibase));
        }
    }

    Ok(blob_bytes)
}

/// Reads `XSQLVAR.relname` into a Rust `String`. Used to capture the
/// relation name that `isc_array_lookup_bounds` needs.
fn xsqlvar_relname(var: &ibase::XSQLVAR) -> String {
    let len = usize::min(var.relname_length as usize, var.relname.len());
    let bytes: Vec<u8> = var.relname[..len].iter().map(|b| *b as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Reads `XSQLVAR.sqlname` into a Rust `String`. Captures the column
/// name as Firebird reported it (un-aliased — `isc_array_lookup_bounds`
/// requires the schema name, not the alias).
fn xsqlvar_sqlname(var: &ibase::XSQLVAR) -> String {
    let len = usize::min(var.sqlname_length as usize, var.sqlname.len());
    let bytes: Vec<u8> = var.sqlname[..len].iter().map(|b| *b as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Decodes a Firebird ARRAY column into a JSON-shaped string. The
/// result is wrapped in `[]` for one-dimensional arrays and nested
/// `[[..],[..]]` for higher-rank arrays.
///
/// Workflow:
/// 1. `isc_array_lookup_bounds` resolves the element type, scale,
///    dimension count, and per-dimension bounds.
/// 2. The total element count + element size determine the slice
///    buffer to allocate.
/// 3. `isc_array_get_slice` copies every element into the buffer.
/// 4. Each element is decoded according to `array_desc_dtype` and
///    formatted as a JSON literal.
fn array_to_string<T: IBase>(
    mut array_id: ibase::GDS_QUAD_t,
    relation: &str,
    column: &str,
    db: &mut ibase::isc_db_handle,
    tr: &mut ibase::isc_tr_handle,
    ibase: &T,
    charset: &Charset,
) -> Result<String, FbError> {
    if relation.is_empty() || column.is_empty() {
        return Ok("[]".into());
    }

    // Names must be NUL-terminated for the C API. Trim trailing
    // whitespace Firebird sometimes pads with for the fixed-width
    // CHAR(32) form, then re-pad with a single NUL.
    let mut rel_buf = relation.trim_end().as_bytes().to_vec();
    rel_buf.push(0);
    let mut col_buf = column.trim_end().as_bytes().to_vec();
    col_buf.push(0);

    let mut desc = ibase::ISC_ARRAY_DESC {
        array_desc_dtype: 0,
        array_desc_scale: 0,
        array_desc_length: 0,
        array_desc_field_name: [0; 32],
        array_desc_relation_name: [0; 32],
        array_desc_dimensions: 0,
        array_desc_flags: 0,
        array_desc_bounds: [ibase::ISC_ARRAY_BOUND {
            array_bound_lower: 0,
            array_bound_upper: 0,
        }; 16],
    };

    let mut status = Status::default();
    unsafe {
        if ibase.isc_array_lookup_bounds()(
            &mut status[0],
            db,
            tr,
            rel_buf.as_ptr() as *const _,
            col_buf.as_ptr() as *const _,
            &mut desc,
        ) != 0
        {
            return Err(status.as_error(ibase));
        }
    }

    let element_size = desc.array_desc_length as usize;
    let mut total_elements: usize = 1;
    let dims = desc.array_desc_dimensions as usize;
    if dims == 0 || dims > 16 {
        return Ok("[]".into());
    }
    let mut shape: Vec<usize> = Vec::with_capacity(dims);
    for i in 0..dims {
        let b = desc.array_desc_bounds[i];
        let span = (b.array_bound_upper as i32 - b.array_bound_lower as i32 + 1).max(0) as usize;
        shape.push(span);
        total_elements = total_elements.saturating_mul(span);
    }
    if total_elements == 0 {
        return Ok("[]".into());
    }

    // VARYING-typed elements ship as `(u16 length) + bytes` slots; the
    // 2 extra bytes per element are reserved by the engine. Account
    // for them here so the allocation never under-provisions.
    // Firebird `dsc_dtype` constants: dtype_varying = 3.
    let slot_size = match desc.array_desc_dtype as u32 {
        3 => element_size + 2,
        _ => element_size,
    };

    let mut slice_len: ibase::ISC_LONG = (slot_size * total_elements) as ibase::ISC_LONG;
    let mut buf = vec![0u8; slot_size * total_elements];

    unsafe {
        if ibase.isc_array_get_slice()(
            &mut status[0],
            db,
            tr,
            &mut array_id,
            &desc,
            buf.as_mut_ptr() as *mut _,
            &mut slice_len,
        ) != 0
        {
            return Err(status.as_error(ibase));
        }
    }

    // Decode each element by the desc's dtype.
    let dtype = desc.array_desc_dtype as u32;
    let scale = desc.array_desc_scale;
    let mut elements: Vec<String> = Vec::with_capacity(total_elements);
    for i in 0..total_elements {
        let off = i * slot_size;
        let raw = &buf[off..off + slot_size];
        elements.push(decode_array_element(dtype, scale, element_size, raw, charset));
    }

    // Render as JSON nested by the shape. For 1-D this is just
    // `[a,b,c]`; for 2-D it becomes `[[a,b],[c,d]]`. Higher dims
    // recurse the same way.
    Ok(format_array(&elements, &shape))
}

/// Decodes a single ARRAY element into its JSON-literal form.
///
/// `dtype` is a Firebird `dsc_dtype` constant (see `dsc.h` in the
/// engine source) — distinct from the BLR wire codes used elsewhere.
/// The most common values are listed below; uncommon ones (BLOB,
/// TIMESTAMP, DECFLOAT) fall through to a placeholder until a real
/// schema needs them.
fn decode_array_element(
    dtype: u32,
    scale: i8,
    length: usize,
    raw: &[u8],
    charset: &Charset,
) -> String {
    use std::convert::TryInto;
    match dtype {
        // dtype_text (CHAR, fixed width)
        1 => {
            let take = length.min(raw.len());
            let s = charset
                .decode(&raw[..take])
                .unwrap_or_else(|_| String::new());
            format!(
                "\"{}\"",
                escape_json(s.trim_end_matches('\0').trim_end().to_string()),
            )
        }
        // dtype_varying (VARCHAR with 2-byte length prefix)
        3 => {
            if raw.len() < 2 {
                "\"\"".into()
            } else {
                let n = u16::from_le_bytes([raw[0], raw[1]]) as usize;
                let take = n.min(raw.len() - 2);
                charset
                    .decode(&raw[2..2 + take])
                    .map(|s| format!("\"{}\"", escape_json(s)))
                    .unwrap_or_else(|_| "\"<bad-encoding>\"".into())
            }
        }
        // dtype_short (SMALLINT, i16)
        8 => {
            let v = i16::from_le_bytes(raw[..2].try_into().unwrap_or([0; 2]));
            apply_scale(v as i64, scale)
        }
        // dtype_long (INTEGER, i32)
        9 => {
            let v = i32::from_le_bytes(raw[..4].try_into().unwrap_or([0; 4]));
            apply_scale(v as i64, scale)
        }
        // dtype_int64 (BIGINT)
        19 => {
            let v = i64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8]));
            apply_scale(v, scale)
        }
        // dtype_real (FLOAT, f32)
        11 => {
            let v = f32::from_le_bytes(raw[..4].try_into().unwrap_or([0; 4]));
            format!("{v}")
        }
        // dtype_double / dtype_d_float (DOUBLE PRECISION, f64)
        12 | 13 => {
            let v = f64::from_le_bytes(raw[..8].try_into().unwrap_or([0; 8]));
            format!("{v}")
        }
        // dtype_boolean (Fb 3+)
        23 => {
            if raw.first().copied().unwrap_or(0) != 0 {
                "true".into()
            } else {
                "false".into()
            }
        }
        // Unknown / date / time / timestamp / blob — surface a
        // placeholder so the row still renders. Real support for those
        // types lands when a user reports a real schema needing it.
        _ => format!("\"<dtype-{dtype}>\""),
    }
}

/// Renders a fixed-point integer using `scale`. Firebird stores
/// NUMERIC(p,s) as `value * 10^|s|` for s < 0; positive scale is
/// uncommon but follows the same multiplier on the other side.
fn apply_scale(v: i64, scale: i8) -> String {
    if scale == 0 {
        return v.to_string();
    }
    if scale < 0 {
        let factor = 10i64.pow((-(scale as i32)) as u32);
        let whole = v / factor;
        let frac = (v % factor).abs();
        let width = (-(scale as i32)) as usize;
        return format!("{whole}.{:0>width$}", frac, width = width);
    }
    let factor = 10i64.pow(scale as u32);
    (v.saturating_mul(factor)).to_string()
}

fn escape_json(s: String) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Renders the flat element list into the JSON array structure
/// implied by `shape`. Row-major layout matches what `isc_array_get_slice`
/// produces.
fn format_array(elements: &[String], shape: &[usize]) -> String {
    fn go(elements: &[String], shape: &[usize], cursor: &mut usize) -> String {
        if shape.len() == 1 {
            let mut parts = Vec::with_capacity(shape[0]);
            for _ in 0..shape[0] {
                parts.push(elements.get(*cursor).cloned().unwrap_or_else(|| "null".into()));
                *cursor += 1;
            }
            format!("[{}]", parts.join(","))
        } else {
            let mut parts = Vec::with_capacity(shape[0]);
            for _ in 0..shape[0] {
                parts.push(go(elements, &shape[1..], cursor));
            }
            format!("[{}]", parts.join(","))
        }
    }
    let mut cursor = 0;
    go(elements, shape, &mut cursor)
}

/// Formats a 16-byte signed little-endian integer as a decimal
/// string. Used for `INT128` (Firebird 4+ SQL type 32752).
fn int128_to_string(bytes: [u8; 16]) -> String {
    i128::from_le_bytes(bytes).to_string()
}

/// Decodes a Firebird `TIME WITH TIME ZONE` cell. Bytes 0..4 hold an
/// `ISC_TIME` (10⁻⁴ s ticks since UTC midnight); bytes 4..6 hold a
/// `uint16` time-zone id (0..1439 = positive UTC offset minutes,
/// 1440..2879 = negative). Rendered as `HH:MM:SS.tttt±HH:MM` or
/// `HH:MM:SS.tttt region:<id>` when the id falls outside the offset
/// range (Firebird stores named regions there).
fn time_tz_to_string(bytes: [u8; 8]) -> String {
    use std::convert::TryInto;
    let isc_time = u32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    let tz_id = u16::from_le_bytes(bytes[4..6].try_into().unwrap_or([0; 2]));
    format!(
        "{} {}",
        format_isc_time(isc_time),
        format_tz_id(tz_id),
    )
}

/// Decodes a Firebird `TIMESTAMP WITH TIME ZONE` cell. Layout: 4-byte
/// `ISC_DATE` + 4-byte `ISC_TIME` + 2-byte time-zone id + 2 bytes of
/// padding.
fn timestamp_tz_to_string(bytes: [u8; 12]) -> String {
    use std::convert::TryInto;
    let isc_date = i32::from_le_bytes(bytes[0..4].try_into().unwrap_or([0; 4]));
    let isc_time = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4]));
    let tz_id = u16::from_le_bytes(bytes[8..10].try_into().unwrap_or([0; 2]));
    let date = rsfbclient_core::date_time::decode_date(isc_date);
    format!(
        "{} {} {}",
        date,
        format_isc_time(isc_time),
        format_tz_id(tz_id),
    )
}

/// Formats an `ISC_TIME` (1/10000-of-a-second ticks since midnight) as
/// `HH:MM:SS.tttt`. Trailing zeros in the fractional component are
/// kept so the layout stays fixed-width.
fn format_isc_time(isc_time: u32) -> String {
    const TICKS_PER_SEC: u32 = 10_000;
    let total_secs = isc_time / TICKS_PER_SEC;
    let frac = isc_time % TICKS_PER_SEC;
    let hours = total_secs / 3600;
    let mins = (total_secs / 60) % 60;
    let secs = total_secs % 60;
    format!("{hours:02}:{mins:02}:{secs:02}.{frac:04}")
}

/// Renders a Firebird timezone id. Per Firebird 4 `TimeZoneUtil`, an
/// offset zone is stored as its displacement in minutes plus
/// `ONE_DAY` (1440), so 1440 is UTC, below it is west of UTC and
/// above it is east. Ids past the offset range are named regions
/// (IANA db), surfaced opaquely — resolving the name requires a call
/// to `MON$TIME_ZONES`, which is out of scope here. That includes
/// Firebird's `GMT_ZONE` (65535), which therefore renders as a region
/// rather than `+00:00`.
fn format_tz_id(id: u16) -> String {
    if id <= 2879 {
        let displacement = i32::from(id) - 1440;
        let sign = if displacement < 0 { '-' } else { '+' };
        let abs = displacement.abs();
        return format!("{sign}{:02}:{:02}", abs / 60, abs % 60);
    }
    format!("region:{id}")
}

/// Renders a Firebird `DECFLOAT(16)` (IEEE 754-2008 decimal64) as a
/// big-endian hex string prefixed `decfloat16:0x`. The decimal value
/// is recoverable via `CAST(<col> AS VARCHAR)` server-side. Full
/// DPD-aware decode would require linking against fbclient's
/// `fb_dec16_*` helpers, which are not yet wired through the IBase
/// trait.
fn dec16_to_string(bytes: [u8; 8]) -> String {
    let mut be = bytes;
    be.reverse();
    let hex: String = be.iter().map(|b| format!("{b:02x}")).collect();
    format!("decfloat16:0x{hex}")
}

/// Renders a Firebird `DECFLOAT(34)` (IEEE 754-2008 decimal128) as a
/// big-endian hex string prefixed `decfloat34:0x`. Same caveat as
/// [`dec16_to_string`].
fn dec34_to_string(bytes: [u8; 16]) -> String {
    let mut be = bytes;
    be.reverse();
    let hex: String = be.iter().map(|b| format!("{b:02x}")).collect();
    format!("decfloat34:0x{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Firebird stores an offset zone as `displacement + ONE_DAY`
    /// (1440), so the pivot is 1440, not 0. Getting this backwards
    /// renders every offset zone with an inverted sign — a +02:00
    /// value reads as -02:00, a four-hour error.
    #[test]
    fn offset_zones_decode_around_the_1440_pivot() {
        assert_eq!(format_tz_id(1440), "+00:00");
        assert_eq!(format_tz_id(1500), "+01:00");
        assert_eq!(format_tz_id(1560), "+02:00");
        assert_eq!(format_tz_id(1380), "-01:00");
        assert_eq!(format_tz_id(1245), "-03:15");
    }

    #[test]
    fn offset_zones_cover_the_full_encodable_range() {
        assert_eq!(format_tz_id(0), "-24:00");
        assert_eq!(format_tz_id(2879), "+23:59");
    }

    #[test]
    fn ids_past_the_offset_range_are_opaque_regions() {
        assert_eq!(format_tz_id(2880), "region:2880");
        assert_eq!(format_tz_id(65535), "region:65535");
    }

    #[test]
    fn isc_time_keeps_fixed_width_fractional_ticks() {
        assert_eq!(format_isc_time(0), "00:00:00.0000");
        assert_eq!(format_isc_time(10_000), "00:00:01.0000");
        assert_eq!(format_isc_time(45_296_500_0), "12:34:56.5000");
    }
}
