//! Versioned vector payloads. Int8 weights and these stored codes are independent.
//! Binary similarity is sign agreement (1 - 2*hamming/d), not FP32 cosine.
use crate::document::{error, Error};

const MAX_DIMENSIONS: usize = 8192;
const HEADER_BYTES: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorFormat {
    Fp32,
    Int8,
    Binary,
}

#[derive(Clone, Debug)]
pub struct StoredVector {
    dimensions: usize,
    data: Data,
}

#[derive(Clone, Debug)]
enum Data {
    Fp32(Vec<f32>),
    Int8 { codes: Vec<i8>, scale: f32 },
    Binary(Vec<u8>),
}

fn valid_vector(values: &[f32]) -> bool {
    !values.is_empty()
        && values.len() <= MAX_DIMENSIONS
        && values.iter().all(|v| v.is_finite())
        && values
            .iter()
            .fold(0.0_f32, |a, b| a.max(b.abs()))
            .is_normal()
}

impl StoredVector {
    /// Int8 uses a symmetric per-vector scale max(abs(x))/127, nearest rounding.
    /// Binary packs low bit first, zero is positive; unused high bits must be zero.
    /// Zero, non-finite and entirely subnormal input vectors are rejected.
    pub fn encode(values: &[f32], format: VectorFormat) -> Result<Self, Error> {
        if !valid_vector(values) {
            return Err(error(
                "vector must contain 1..8192 finite dimensions with normal magnitude",
            ));
        }
        let data = match format {
            VectorFormat::Fp32 => Data::Fp32(values.to_vec()),
            VectorFormat::Int8 => {
                let scale = values.iter().fold(0.0_f32, |a, b| a.max(b.abs())) / 127.;
                if !scale.is_finite() || scale <= 0. {
                    return Err(error("unrepresentable vector scale"));
                }
                let codes: Vec<i8> = values
                    .iter()
                    .map(|v| (v / scale).round().clamp(-127., 127.) as i8)
                    .collect();
                // Rounding a near-maximum f32 scale can overflow on decode.
                // Every accepted encoding must satisfy the strict reader too.
                let decoded: Vec<_> = codes.iter().map(|&v| v as f32 * scale).collect();
                if !valid_vector(&decoded) {
                    return Err(error("quantized vector magnitude cannot be represented"));
                }
                Data::Int8 { scale, codes }
            }
            VectorFormat::Binary => {
                let mut bits = vec![0; values.len().div_ceil(8)];
                for (i, &value) in values.iter().enumerate() {
                    if value >= 0. {
                        bits[i / 8] |= 1 << (i % 8);
                    }
                }
                Data::Binary(bits)
            }
        };
        Ok(Self {
            dimensions: values.len(),
            data,
        })
    }

    pub fn format(&self) -> VectorFormat {
        match self.data {
            Data::Fp32(_) => VectorFormat::Fp32,
            Data::Int8 { .. } => VectorFormat::Int8,
            Data::Binary(_) => VectorFormat::Binary,
        }
    }
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }
    /// Includes the Int8 scale; excludes the 9-byte wire header, allocator and text.
    pub fn payload_bytes(&self) -> usize {
        match &self.data {
            Data::Fp32(v) => 4 * v.len(),
            Data::Int8 { codes, .. } => codes.len() + 4,
            Data::Binary(v) => v.len(),
        }
    }
    pub fn decode(&self) -> Vec<f32> {
        match &self.data {
            Data::Fp32(v) => v.clone(),
            Data::Int8 { codes, scale } => codes.iter().map(|&v| v as f32 * scale).collect(),
            Data::Binary(bits) => (0..self.dimensions)
                .map(|i| {
                    if bits[i / 8] & (1 << (i % 8)) != 0 {
                        1.
                    } else {
                        -1.
                    }
                })
                .collect(),
        }
    }
    /// Compatible-format cosine for FP32/Int8; sign agreement for binary.
    /// Int8 scales cancel in cosine; norms are recomputed from quantized codes.
    /// No allocation or full FP32 reconstruction is needed for comparison.
    pub fn similarity(&self, other: &Self) -> Result<f32, Error> {
        if self.dimensions != other.dimensions || self.format() != other.format() {
            return Err(error("incompatible vector dimensions or formats"));
        }
        fn cosine(a: impl Iterator<Item = f64>, b: impl Iterator<Item = f64>) -> f32 {
            let (mut dot, mut nx, mut ny) = (0., 0., 0.);
            for (x, y) in a.zip(b) {
                dot += x * y;
                nx += x * x;
                ny += y * y;
            }
            (dot / (nx.sqrt() * ny.sqrt())).clamp(-1., 1.) as f32
        }
        Ok(match (&self.data, &other.data) {
            (Data::Fp32(a), Data::Fp32(b)) => {
                cosine(a.iter().map(|&v| v as f64), b.iter().map(|&v| v as f64))
            }
            (Data::Int8 { codes: a, .. }, Data::Int8 { codes: b, .. }) => {
                cosine(a.iter().map(|&v| v as f64), b.iter().map(|&v| v as f64))
            }
            (Data::Binary(a), Data::Binary(b)) => {
                let differing: u32 = a.iter().zip(b).map(|(&a, &b)| (a ^ b).count_ones()).sum();
                1. - 2. * differing as f32 / self.dimensions as f32
            }
            _ => unreachable!("matching formats checked above"),
        })
    }
    /// RV01 + format:u8 + dimensions:u32LE + payload. Strict length, no padding.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + self.payload_bytes());
        bytes.extend_from_slice(b"RV01");
        bytes.push(match self.format() {
            VectorFormat::Fp32 => 0,
            VectorFormat::Int8 => 1,
            VectorFormat::Binary => 2,
        });
        bytes.extend_from_slice(&(self.dimensions as u32).to_le_bytes());
        match &self.data {
            Data::Fp32(v) => {
                for value in v {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
            Data::Int8 { codes, scale } => {
                bytes.extend_from_slice(&scale.to_le_bytes());
                bytes.extend(codes.iter().map(|&v| v as u8));
            }
            Data::Binary(v) => bytes.extend_from_slice(v),
        }
        bytes
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let bad = || error("invalid stored vector encoding");
        if bytes.len() < HEADER_BYTES || &bytes[..4] != b"RV01" {
            return Err(bad());
        }
        let dimensions = u32::from_le_bytes(bytes[5..9].try_into().map_err(|_| bad())?) as usize;
        if dimensions == 0 || dimensions > MAX_DIMENSIONS {
            return Err(bad());
        }
        let payload = &bytes[HEADER_BYTES..];
        let data = match bytes[4] {
            0 if payload.len() == dimensions * 4 => {
                let v: Vec<_> = payload
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().expect("four byte chunk")))
                    .collect();
                if !valid_vector(&v) {
                    return Err(bad());
                }
                Data::Fp32(v)
            }
            1 if payload.len() == dimensions + 4 => {
                let scale = f32::from_le_bytes(payload[..4].try_into().map_err(|_| bad())?);
                let codes: Vec<_> = payload[4..].iter().map(|&v| v as i8).collect();
                if !scale.is_finite()
                    || scale <= 0.
                    || codes.contains(&-128)
                    || !codes.iter().any(|&v| v != 0)
                {
                    return Err(bad());
                }
                // Reject imported payloads whose decoded values overflow or underflow.
                let decoded: Vec<_> = codes.iter().map(|&v| v as f32 * scale).collect();
                if !valid_vector(&decoded) {
                    return Err(bad());
                }
                Data::Int8 { codes, scale }
            }
            2 if payload.len() == dimensions.div_ceil(8) => {
                if !dimensions.is_multiple_of(8)
                    && payload[payload.len() - 1] >> (dimensions % 8) != 0
                {
                    return Err(bad());
                }
                Data::Binary(payload.to_vec())
            }
            _ => return Err(bad()),
        };
        Ok(Self { dimensions, data })
    }
}
