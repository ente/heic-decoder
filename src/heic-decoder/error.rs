use alloc::string::String;
use core::fmt;

#[derive(Debug)]
#[non_exhaustive]
pub enum HevcError {
    AllocationFailed,
    InvalidNalUnit(&'static str),
    InvalidBitstream(&'static str),
    MissingParameterSet(&'static str),
    InvalidParameterSet { kind: &'static str, msg: String },
    CabacError(&'static str),
    UnsupportedProfile { profile: u8, level: u8 },
    Unsupported(&'static str),
    DecodingError(&'static str),
}

impl fmt::Display for HevcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AllocationFailed => write!(f, "allocation failed"),
            Self::InvalidNalUnit(msg) => write!(f, "invalid NAL unit: {msg}"),
            Self::InvalidBitstream(msg) => write!(f, "invalid bitstream: {msg}"),
            Self::MissingParameterSet(kind) => write!(f, "missing {kind}"),
            Self::InvalidParameterSet { kind, msg } => {
                write!(f, "invalid {kind}: {msg}")
            }
            Self::CabacError(msg) => write!(f, "CABAC error: {msg}"),
            Self::UnsupportedProfile { profile, level } => {
                write!(f, "unsupported profile {profile} level {level}")
            }
            Self::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Self::DecodingError(msg) => write!(f, "decoding error: {msg}"),
        }
    }
}

impl core::error::Error for HevcError {}
