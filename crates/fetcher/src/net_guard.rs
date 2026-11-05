//! プライベートネットワーク宛ての接続を拒否する SSRF 対策。
//!
//! MCP 経由では URL を LLM（= 信頼できない入力）が決めるため、ローカルの管理画面や
//! クラウドのメタデータエンドポイント（169.254.169.254）に到達させない。
//! DNS リバインディング（検査時と接続時で別の IP を返す）を避けるため、URL 文字列の
//! ホスト名ではなく、reqwest が実際に接続に使う DNS 解決結果の IP を検査する。

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use url::{Host, Url};

/// 拒否理由を運ぶエラー型。
///
/// reqwest のエラーは resolver / redirect policy が返したエラーを source として保持するため、
/// 呼び出し側で source chain を downcast してこの型を見つけ、`FetchError::Blocked` に変換する。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct BlockedTarget(pub(crate) String);

/// 接続を拒否すべき IP か判定する。
///
/// IPv4-mapped / IPv4-compatible の IPv6 アドレスは埋め込まれた IPv4 で判定する。
/// `::ffff:127.0.0.1` のような表記でループバックへ到達できてしまうため。
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ipv4(v4);
            }
            is_blocked_ipv6(v6)
        }
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        // 0.0.0.0/8 は Linux では 0.0.0.0 と同様にローカルホストへ接続される
        || a == 0
        // 100.64.0.0/10（CGNAT）。Tailscale 等の社内 VPN がこの帯域を使う
        || (a == 100 && (b & 0xC0) == 64)
        // 192.0.0.0/24（IETF プロトコル割当）
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15（ベンチマーク計測用。内部ネットワークに割り当てられる）
        || (a == 198 && (b == 18 || b == 19))
        // 224.0.0.0/4 マルチキャストと 240.0.0.0/4 予約帯域。公開 Web サーバーがこの帯域に存在することはない
        || ip.is_multicast()
        || a >= 240
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    let first = seg[0];
    ip.is_loopback()
        || ip.is_unspecified()
        // fe80::/10 リンクローカル
        || (first & 0xFFC0) == 0xFE80
        // fc00::/7 ユニークローカル（ULA）
        || (first & 0xFE00) == 0xFC00
        // ff00::/8 マルチキャスト
        || (first & 0xFF00) == 0xFF00
        // 64:ff9b::/96 NAT64。NAT64 ゲートウェイのあるネットワークでは埋め込まれた IPv4 へ変換されて到達する
        || (seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] && is_blocked_ipv4(ipv4_tail(ip)))
        // 64:ff9b:1::/48 ローカル用 NAT64（RFC 8215）
        || (first == 0x64 && seg[1] == 0xff9b && seg[2] == 1)
        // 2002::/16 6to4。埋め込まれた IPv4 が拒否対象ならリレー経由で到達する
        || (first == 0x2002 && is_blocked_ipv4(six_to_four_tail(ip)))
        // ::/96 IPv4-compatible（非推奨形式だが ::127.0.0.1 として解釈する実装がある）
        || (seg[..6].iter().all(|s| *s == 0) && is_blocked_ipv4(ipv4_tail(ip)))
}

/// 6to4 (2002:AABB:CCDD::/48) に埋め込まれた IPv4 を取り出す。
fn six_to_four_tail(ip: Ipv6Addr) -> Ipv4Addr {
    let s = ip.segments();
    Ipv4Addr::new(
        (s[1] >> 8) as u8,
        (s[1] & 0xFF) as u8,
        (s[2] >> 8) as u8,
        (s[2] & 0xFF) as u8,
    )
}

fn ipv4_tail(ip: Ipv6Addr) -> Ipv4Addr {
    let o = ip.octets();
    Ipv4Addr::new(o[12], o[13], o[14], o[15])
}

/// URL のスキームと、IP リテラルで書かれたホストを検査する。
///
/// IP リテラルのホストは DNS 解決を経ないため resolver での検査が働かない。
/// そのため送信前とリダイレクトの各ホップでこの関数を呼ぶ。
/// ホスト名の場合はここでは判定せず、resolver 側で解決結果の IP を検査する。
pub fn check_url(url: &Url, deny_private: bool) -> Result<(), String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("unsupported scheme: {}", other)),
    }
    if !deny_private {
        return Ok(());
    }
    let ip = match url.host() {
        Some(Host::Ipv4(v4)) => IpAddr::V4(v4),
        Some(Host::Ipv6(v6)) => IpAddr::V6(v6),
        Some(Host::Domain(_)) => return Ok(()),
        None => return Err("URL has no host".to_owned()),
    };
    if is_blocked_ip(ip) {
        return Err(format!(
            "private network address {} is not allowed (set STILLO_ALLOW_PRIVATE_NETWORK=1 to allow private hosts)",
            ip
        ));
    }
    Ok(())
}

/// 解決結果に拒否対象の IP が1つでも含まれれば失敗させる DNS resolver。
///
/// 拒否対象だけを除外して残りで接続する方式は取らない。公開 IP とプライベート IP を
/// 混ぜて返すドメインは意図的な攻撃である可能性が高く、部分的に許可する理由がないため。
pub(crate) struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // ポートは reqwest 側で URL のものに差し替えられるため 0 でよい
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(bad) = addrs.iter().find(|a| is_blocked_ip(a.ip())) {
                return Err(Box::new(BlockedTarget(format!(
                    "{} resolves to private network address {}",
                    host,
                    bad.ip()
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// エラーの source chain から `BlockedTarget` を探す。
pub(crate) fn find_blocked(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(b) = e.downcast_ref::<BlockedTarget>() {
            return Some(b.0.clone());
        }
        cur = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_private_and_local_ranges() {
        for s in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "100.64.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "224.0.0.1",
            "239.255.255.250",
            "240.0.0.1",
            // 192.0.0.0/24・198.18.0.0/15
            "192.0.0.1",
            "198.18.0.1",
            "198.19.255.255",
            "ff02::1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
            // ローカル用 NAT64 (64:ff9b:1::/48)
            "64:ff9b:1::1",
            // 6to4 の埋め込み IPv4 が loopback / private
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for s in [
            "8.8.8.8",
            "93.184.216.34",
            "172.32.0.1",
            "100.128.0.1",
            // NAT64 経由の公開 IPv4（8.8.8.8）は許可する
            "64:ff9b::808:808",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "fec0::1",
            // 6to4 の埋め込み IPv4 が公開アドレスなら許可
            "2002:808:808::1",
            // 198.18/15 の外側
            "198.20.0.1",
            "192.0.1.1",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn check_url_rejects_non_http_scheme_even_when_allowing_private() {
        let url = Url::parse("file:///etc/passwd").unwrap();
        assert!(check_url(&url, false).is_err());
        let url = Url::parse("ftp://example.com/").unwrap();
        assert!(check_url(&url, true).is_err());
    }

    #[test]
    fn check_url_inspects_ip_literal_hosts_only_when_denying() {
        let v4 = Url::parse("http://127.0.0.1:8080/").unwrap();
        let v6 = Url::parse("http://[::ffff:7f00:1]/").unwrap();
        let meta = Url::parse("http://169.254.169.254/latest/meta-data/").unwrap();
        assert!(check_url(&v4, true).is_err());
        assert!(check_url(&v6, true).is_err());
        assert!(check_url(&meta, true).is_err());
        assert!(check_url(&v4, false).is_ok());
        // ホスト名は resolver 側で検査するためここでは通す
        let domain = Url::parse("http://localhost/").unwrap();
        assert!(check_url(&domain, true).is_ok());
        let public = Url::parse("https://8.8.8.8/").unwrap();
        assert!(check_url(&public, true).is_ok());
    }
}
