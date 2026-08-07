//! 多因子设备指纹（bfp）。
//!
//! 浏览器前端（common-header.js）计算 32 位大写 hex 指纹并 `GET /bfp/info?bfp=<hex>`
//! 上报，服务端种下 HttpOnly 持久 cookie `MULTIFACTOR_BROWSER_FINGERPRINT`，作为
//! 多因子风控的可信设备指纹。分析实测（2026-08-06）：**服务端只把上报值当字符串
//! 存储比对，不校验算法**——自动化自定 32 位大写 hex 即可；且 live 全链路实测
//! 证明不发 bfp 也能完成登录 + reauth，上报仅是贴近浏览器行为、降低风控误判风险。

/// 生成 32 位大写 hex 设备指纹。
pub fn random_fingerprint() -> String {
    use rand::Rng;
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut rng = rand::rng();
    (0..32)
        .map(|_| HEX[rng.random_range(0..16)] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_32_upper_hex() {
        let fp = random_fingerprint();
        assert_eq!(fp.len(), 32);
        // 全部是 hex 字符，且字母部分必须大写（数字没有大小写）
        assert!(
            fp.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        );
    }

    #[test]
    fn fingerprints_differ() {
        assert_ne!(random_fingerprint(), random_fingerprint());
    }
}
