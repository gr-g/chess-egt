mod indexer;
pub mod piece_set;
pub mod egt;

use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionType {
    Capture,
    Promotion,
    Checkmate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WdlOutcome {
    Loss(ConversionType),
    Draw,
    Win(ConversionType),
}

impl Ord for WdlOutcome {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (WdlOutcome::Win(_), WdlOutcome::Draw) => Ordering::Greater,
            (WdlOutcome::Draw, WdlOutcome::Win(_)) => Ordering::Less,
            (WdlOutcome::Win(_), WdlOutcome::Loss(_)) => Ordering::Greater,
            (WdlOutcome::Loss(_), WdlOutcome::Win(_)) => Ordering::Less,
            (WdlOutcome::Draw, WdlOutcome::Loss(_)) => Ordering::Greater,
            (WdlOutcome::Loss(_), WdlOutcome::Draw) => Ordering::Less,
            _ => Ordering::Equal,
        }
    }
}

impl PartialOrd for WdlOutcome {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtcOutcome {
    Win(ConversionType, u16),
    Draw,
    Loss(ConversionType, u16),
}

impl Ord for DtcOutcome {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (DtcOutcome::Win(ct1, n1), DtcOutcome::Win(ct2, n2)) => n2.cmp(n1).then(ct1.cmp(ct2)),
            (DtcOutcome::Draw, DtcOutcome::Draw) => Ordering::Equal,
            (DtcOutcome::Loss(ct1, n1), DtcOutcome::Loss(ct2, n2)) => n1.cmp(n2).then(ct2.cmp(ct1)),
            (DtcOutcome::Win(_, _), DtcOutcome::Draw) => Ordering::Greater,
            (DtcOutcome::Draw, DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Win(_, _), DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Win(_, _)) => Ordering::Less,
            (DtcOutcome::Draw, DtcOutcome::Loss(_, _)) => Ordering::Greater,
            (DtcOutcome::Loss(_, _), DtcOutcome::Draw) => Ordering::Less,
        }
    }
}

impl PartialOrd for DtcOutcome {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

impl DtcOutcome {
    pub fn from_u16(value: u16) -> Self {
        let n = value >> 3;
        match value & 0x111 {
            0b000 => panic!("invalid value used to create DtcOutcome"),
            0b001 => Self::Draw,
            0b010 => Self::Win(ConversionType::Checkmate, n),
            0b100 => Self::Win(ConversionType::Promotion, n),
            0b110 => Self::Win(ConversionType::Capture, n),
            0b011 => Self::Loss(ConversionType::Checkmate, n),
            0b101 => Self::Loss(ConversionType::Promotion, n),
            0b111 => Self::Loss(ConversionType::Capture, n),
            _ => unreachable!(),
        }
    }

    pub fn to_u16(&self) -> u16 {
        match self {
            Self::Draw => 0b001,
            Self::Win(ConversionType::Checkmate, n) => 0b010 + n << 3,
            Self::Win(ConversionType::Promotion, n) => 0b100 + n << 3,
            Self::Win(ConversionType::Capture, n) => 0b0110 + n << 3,
            Self::Loss(ConversionType::Checkmate, n) => 0b011 + n << 3,
            Self::Loss(ConversionType::Promotion, n) => 0b101 + n << 3,
            Self::Loss(ConversionType::Capture, n) => 0b0111 + n << 3,
        }
    }
}

pub struct EgtGenerator {
    path: PathBuf,
    save_wdl_oneside: bool,
    save_dtc_oneside: bool,
}

impl EgtGenerator {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            save_wdl_oneside: false,
            save_dtc_oneside: false,
        }
    }

    pub fn set_wdl_oneside(&mut self, val: bool) { self.save_wdl_oneside = val; }
    pub fn set_dtc_oneside(&mut self, val: bool) { self.save_dtc_oneside = val; }

    pub fn generate(&self, tablename: &str) {
        println!("Generating table {} at {:?}", tablename, self.path);
        // Implementation goes here
    }
}

pub struct EgtProber {
    path: PathBuf,
}

impl EgtProber {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    //pub fn probe_wdl(&self, board: &Board) -> WdlOutcome {
        // Implementation goes here
    //    WdlOutcome::Draw
    //}

    //pub fn probe_dtc(&self, board: &Board) -> DtcOutcome {
        // Implementation goes here
    //    DtcOutcome::Draw
    //}
}
