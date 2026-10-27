//! 地址模型：`Func` / `Type` / `Order` / `PointSpec` + 地址串解析 + func 命名/功能码。

/// 功能码（4 种读各对应独立地址空间，合并时按 func 分组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Func {
    /// FC01 coil（可读可写，1 bit/addr）。
    Coil,
    /// FC02 discrete input（只读，1 bit/addr）。
    Discrete,
    /// FC03 holding register（可读可写，16 bit/addr）。
    Holding,
    /// FC04 input register（只读，16 bit/addr）。
    Input,
}

impl Func {
    /// 是否只读（discrete/input）。
    pub(super) fn is_readonly(self) -> bool {
        matches!(self, Func::Discrete | Func::Input)
    }
}

/// 数据类型。`Bool` 仅配 coil/discrete（直接布尔）；`Bit` 仅配 holding/input（寄存器位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Type {
    /// coil/discrete 直接布尔（FC01/FC02 返回 `Vec<bool>`）。
    Bool,
    /// holding/input 寄存器位（需位号 0-15）。
    Bit,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

/// 32/64 位浮点/整型的字节序（Modicon 标准记法）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Order {
    /// 大端（默认，ABCD）。
    Abcd,
    /// 字交换（CDAB，low word first）。
    Cdab,
    /// 字内字节交换（BADC）。
    Badc,
    /// 小端（DCBA）。
    Dcba,
}

/// 一个点位规约（解析自地址串）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PointSpec {
    pub(super) func: Func,
    pub(super) addr: u16,
    pub(super) typ: Type,
    pub(super) order: Order,
    pub(super) bit: Option<u8>,
}

impl PointSpec {
    /// 该点占用的地址单元数（coil/discrete bool=1 bit 地址；holding/input bit/i16/u16=1 寄存器，
    /// i32/u32/f32=2，f64=4）。
    pub(super) fn width(&self) -> u16 {
        match self.typ {
            Type::Bool | Type::Bit | Type::I16 | Type::U16 => 1,
            Type::I32 | Type::U32 | Type::F32 => 2,
            Type::F64 => 4,
        }
    }
}

pub(super) fn parse_func(s: &str) -> Result<Func, String> {
    match s.to_lowercase().as_str() {
        "coil" | "coils" | "fc01" => Ok(Func::Coil),
        "discrete" | "discretes" | "discrete_input" | "fc02" => Ok(Func::Discrete),
        "holding" | "holdings" | "fc03" => Ok(Func::Holding),
        "input" | "inputs" | "fc04" => Ok(Func::Input),
        other => Err(format!(
            "unsupported func '{other}'（coil/discrete/holding/input）"
        )),
    }
}

pub(super) fn parse_type(s: &str) -> Result<Type, String> {
    match s.to_lowercase().as_str() {
        "bool" | "boolean" => Ok(Type::Bool),
        "bit" => Ok(Type::Bit),
        "i16" | "int16" => Ok(Type::I16),
        "u16" | "uint16" => Ok(Type::U16),
        "i32" | "int32" => Ok(Type::I32),
        "u32" | "uint32" => Ok(Type::U32),
        "f32" | "float" | "float32" => Ok(Type::F32),
        "f64" | "double" | "float64" => Ok(Type::F64),
        other => Err(format!("unsupported type '{other}'")),
    }
}

pub(super) fn parse_order(s: &str) -> Result<Order, String> {
    match s.to_uppercase().as_str() {
        "ABCD" | "BE" | "BIG" => Ok(Order::Abcd),
        "CDAB" | "SWAP" | "WORDSWAP" => Ok(Order::Cdab),
        "BADC" | "BYTESWAP" => Ok(Order::Badc),
        "DCBA" | "LE" | "LITTLE" => Ok(Order::Dcba),
        other => Err(format!(
            "unsupported byte_order '{other}'（ABCD/CDAB/BADC/DCBA）"
        )),
    }
}

/// 解析地址串 `<func>:<addr>:<type>[:<byte_order>][:<bit>]`。
///
/// func 与 type 组合校验：coil/discrete 仅 bool（直接布尔，无后续段）；holding/input 用
/// bit（需位号）/i16/u16（无字节序）/i32/u32/f32/f64（可选字节序），不接受 bool。
pub(super) fn parse_point(s: &str) -> Result<PointSpec, String> {
    let parts: Vec<&str> = s.split(':').collect();
    let func = parse_func(parts.first().copied().unwrap_or(""))?;
    let addr = parts
        .get(1)
        .ok_or_else(|| "missing addr".to_string())
        .and_then(|a| a.parse::<u16>().map_err(|_| format!("invalid addr '{a}'")))?;
    let typ = parse_type(parts.get(2).ok_or_else(|| "missing type".to_string())?)?;
    let mut order = Order::Abcd;
    let mut bit = None;
    match (func, typ) {
        (Func::Coil | Func::Discrete, Type::Bool) => {
            if parts.len() > 3 {
                return Err("coil/discrete 不支持字节序/位号段（直接布尔）".to_string());
            }
        }
        (Func::Coil | Func::Discrete, _) => {
            return Err("coil/discrete 只接受 bool 类型（FC01/FC02 返回布尔）".to_string());
        }
        (Func::Holding | Func::Input, Type::Bool) => {
            return Err("holding/input 用 bit（寄存器位解析）而非 bool".to_string());
        }
        (Func::Holding | Func::Input, Type::Bit) => {
            let b = parts
                .get(3)
                .ok_or_else(|| "bit type requires bit number (0-15)".to_string())
                .and_then(|x| x.parse::<u8>().map_err(|_| format!("invalid bit '{x}'")))?;
            if b > 15 {
                return Err(format!("bit {b} out of range (0-15)"));
            }
            bit = Some(b);
        }
        (Func::Holding | Func::Input, Type::I16 | Type::U16) => {
            if parts.len() > 3 {
                return Err(format!(
                    "{} 不支持字节序段（16 位单寄存器）",
                    if typ == Type::I16 { "i16" } else { "u16" }
                ));
            }
        }
        (Func::Holding | Func::Input, Type::I32 | Type::U32 | Type::F32 | Type::F64) => {
            if let Some(o) = parts.get(3) {
                order = parse_order(o)?;
            }
        }
    }
    Ok(PointSpec {
        func,
        addr,
        typ,
        order,
        bit,
    })
}

/// func 的可读名（日志用）。
pub(super) fn addr_func_name(f: Func) -> &'static str {
    match f {
        Func::Coil => "coil",
        Func::Discrete => "discrete",
        Func::Holding => "holding",
        Func::Input => "input",
    }
}

/// func 的功能码号（日志用）。
pub(super) fn addr_fc(f: Func) -> u8 {
    match f {
        Func::Coil => 1,
        Func::Discrete => 2,
        Func::Holding => 3,
        Func::Input => 4,
    }
}
