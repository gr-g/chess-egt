mod indexer;
pub mod piece_set;
pub mod egt;
pub mod egt_file;

use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConversionType {
    Promotion,
    Capture,
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
        MaybeDtcOutcome::from_u16(value).unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaybeDtcOutcome(pub u16);

impl MaybeDtcOutcome {
    pub const INVALID: Self = Self(0b000);
    pub const DRAW: Self = Self(0b001);
    pub const UNKNOWN: Self = Self(0b1111111111111000);

    pub fn from_u16(value: u16) -> Self {
        Self(value)
    }

    pub fn to_u16(&self) -> u16 {
        self.0
    }

    pub fn is_invalid(&self) -> bool {
        self.0 == 0b000
    }

    pub fn is_unknown(&self) -> bool {
        (self.0 & 0b111) == 0b000 && (self.0 >> 3) != 0
    }

    pub fn is_draw(&self) -> bool {
        self.0 == 0b001
    }

    pub fn is_win(&self) -> bool {
        match self.0 & 0b111 {
            0b010 | 0b100 | 0b110 => true,
            _ => false,
        }
    }

    pub fn is_loss(&self) -> bool {
        match self.0 & 0b111 {
            0b011 | 0b101 | 0b111 => true,
            _ => false,
        }
    }

    pub fn new_win(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b010,
            ConversionType::Capture => 0b100,
            ConversionType::Promotion => 0b110,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_loss(ct: ConversionType, distance: u16) -> Self {
        let bits = match ct {
            ConversionType::Checkmate => 0b011,
            ConversionType::Capture => 0b101,
            ConversionType::Promotion => 0b111,
        };
        Self(bits | (distance << 3))
    }

    pub fn new_unknown(moves_counter: u16) -> Self {
        Self(moves_counter << 3)
    }

    pub fn unwrap(self) -> DtcOutcome {
        let n = self.0 >> 3;
        match self.0 & 0b111 {
            0b000 if n == 0 => panic!("invalid value used to create DtcOutcome"),
            0b000 if n != 0 => panic!("unknown value used to create DtcOutcome"),
            0b001 => DtcOutcome::Draw,
            0b010 => DtcOutcome::Win(ConversionType::Checkmate, n),
            0b100 => DtcOutcome::Win(ConversionType::Capture, n),
            0b110 => DtcOutcome::Win(ConversionType::Promotion, n),
            0b011 => DtcOutcome::Loss(ConversionType::Checkmate, n),
            0b101 => DtcOutcome::Loss(ConversionType::Capture, n),
            0b111 => DtcOutcome::Loss(ConversionType::Promotion, n),
            _ => unreachable!(),
        }
    }
}

#[allow(dead_code)]
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

        // Derive the file path from the folder path and tablename
        let file_path = self.path.join(format!("{}.egt", tablename));

        // Create an EgtFile from scratch
        let mut egt_file = crate::egt_file::EgtFile::new(file_path, tablename, true)
            .expect("Failed to create EgtFile");

        // Create a temporary Arena (e.g., 4GB capacity)
        let mut arena = crate::egt_file::Arena::new(4 * 1024 * 1024 * 1024);

        // Generate random outcomes
        egt_file.generate_random_outcomes(&mut arena);

        // Save to disk
        egt_file.save_to_disk(&mut arena).expect("Failed to flush EgtFile");

        println!(
            "Successfully generated table {} with {} positions.",
            tablename,
            egt_file.total_positions()
        );
    }
}

#[allow(dead_code)]
pub struct EgtProber {
    path: PathBuf,
}

impl EgtProber {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    //pub fn probe_wdl(&self, board: &shakmaty::Setup) -> WdlOutcome {
        // Implementation goes here
    //    WdlOutcome::Draw
    //}

    //pub fn probe_dtc(&self, board: &shakmaty::Setup) -> DtcOutcome {
        // Implementation goes here
    //    DtcOutcome::Draw
    //}
}
