//! 主数据编码生成(M1 stub / M8 接 cmx-code)。
//!
//! 激活器新建主数据时生成 code。通过 [`CodeGenerator`] trait 解耦:cmx-code 就绪前用
//! [`RandomCodeGenerator`] stub,就绪后换 `CmxCodeGenerator` 实现,激活器主流程零改。

/// 编码生成器 trait(激活器依赖此抽象,M8 替换实现)。
pub trait CodeGenerator: Send + Sync {
    /// 为新建主数据生成 code。
    ///
    /// - `dict_code`:目标字典码(如 supplier),用于前缀/规则定位
    /// - `rule_code`:`mdm_activation.codeRuleCode`(M8 时传给 cmx-code)
    fn generate(&self, dict_code: &str, rule_code: Option<&str>) -> String;
}

/// M1 临时实现:前缀 + 雪花 id 转 36 进制(M8 替换为 cmx-code 规则调用)。
///
/// 格式:`<DICT 大写前缀>-<雪花 id 36 进制>`(如 `SUPPLI-LS3KQ7A2`)。
/// **无外部依赖**:用 `cmx_utils::next_pk_id()`(i64)转 36 进制作随机段,既保证唯一性(雪花 id
/// 全局唯一)又免去引入 rand crate。唯一性兜底:cm_*.code 有 UNIQUE 约束,冲突时激活器重试。
pub struct RandomCodeGenerator;

/// [`CodeGenerator`] 的 M1 临时实现。
impl CodeGenerator for RandomCodeGenerator {
    fn generate(&self, dict_code: &str, _rule_code: Option<&str>) -> String {
        let upper = dict_code.to_uppercase();
        // 截取前 6 字符作前缀,避免过长(char 边界安全)
        let mut prefix: String = upper.chars().take(6).collect();
        if prefix.is_empty() {
            prefix = "MDM".to_string();
        }
        // next_pk_id() 全局唯一,转 36 进制得到紧凑随机串
        let rand = format_radix(cmx_utils::next_pk_id(), 36);
        format!("{prefix}-{rand}")
    }
}

/// i64 转 base-N 字符串(小写,无前缀)。
fn format_radix(mut n: i64, radix: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(digits[(n % radix as i64) as usize]);
        n /= radix as i64;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_else(|_| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_non_empty_code_with_prefix_and_separator() {
        let g = RandomCodeGenerator;
        let code = g.generate("supplier", None);
        assert!(code.starts_with("SUPPLI-"), "code={code}");
        // 分隔符后非空
        let rand_part = code.split('-').nth(1).unwrap();
        assert!(!rand_part.is_empty());
    }

    #[test]
    fn generates_unique_codes() {
        let g = RandomCodeGenerator;
        let a = g.generate("supplier", None);
        let b = g.generate("supplier", None);
        // next_pk_id 单调递增,两次必然不同
        assert_ne!(a, b);
    }

    #[test]
    fn empty_dict_code_falls_back_to_mdm_prefix() {
        let g = RandomCodeGenerator;
        let code = g.generate("", None);
        assert!(code.starts_with("MDM-"), "code={code}");
    }

    #[test]
    fn format_radix_basic() {
        assert_eq!(format_radix(0, 36), "0");
        assert_eq!(format_radix(35, 36), "z");
        assert_eq!(format_radix(36, 36), "10");
    }
}
