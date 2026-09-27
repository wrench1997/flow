//! Network configuration, local checks and truthful source-stage explanations.
use crate::settings::Settings;
use serde_json::{Value, json};

pub fn configuration(s: &Settings) -> Value {
    json!({"ipv6_enabled":s.ipv6_enabled,"tcp_enabled":s.tcp_enabled,"utp_enabled":s.utp_enabled,
        "listen_port":s.listen_port,"upnp_enabled":s.upnp_enabled,"peer_limit":s.peer_limit})
}

pub fn listener(s: &Settings, offline: bool) -> (librqbit::ListenerOptions, bool, Vec<String>) {
    let mut warnings = Vec::new();
    let ipv6 = s.ipv6_enabled && std::net::TcpListener::bind("[::1]:0").is_ok();
    if s.ipv6_enabled && !ipv6 {
        warnings.push("本机 IPv6 不可用，已回退 IPv4".into());
    }
    let mode = match (s.tcp_enabled, s.utp_enabled) {
        (true, true) => librqbit::ListenerMode::TcpAndUtp,
        (false, true) => librqbit::ListenerMode::UtpOnly,
        _ => librqbit::ListenerMode::TcpOnly,
    };
    (
        librqbit::ListenerOptions {
            mode,
            listen_addr: if ipv6 {
                ([0u16; 8], if offline { 0 } else { s.listen_port }).into()
            } else {
                ([0u8; 4], if offline { 0 } else { s.listen_port }).into()
            },
            ipv4_only: !ipv6,
            allow_port_fallback: !offline,
            enable_upnp_port_forwarding: s.upnp_enabled && !offline,
            ..Default::default()
        },
        !ipv6,
        warnings,
    )
}

pub async fn check_local(session: &librqbit::Session) -> Value {
    let diagnostics = session.network_diagnostics();
    let Some(address) = session.listen_addr() else {
        return json!({"error":"没有监听端口"});
    };
    let mut checks = Vec::new();
    if diagnostics["tcp_enabled"] == true {
        let mut addrs = vec![std::net::SocketAddr::from(([127, 0, 0, 1], address.port()))];
        if !session.ipv4_only {
            addrs.push(([0, 0, 0, 0, 0, 0, 0, 1], address.port()).into());
        }
        for addr in addrs {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(750),
                tokio::net::TcpStream::connect(addr),
            )
            .await;
            checks.push(match result {
                Ok(Ok(stream)) => {
                    drop(stream);
                    json!({"address":addr.to_string(),"ok":true})
                }
                Ok(Err(error)) => {
                    json!({"address":addr.to_string(),"ok":false,"error":error.to_string()})
                }
                Err(_) => json!({"address":addr.to_string(),"ok":false,"error":"本机连接超时"}),
            });
        }
    }
    json!({"checked_at":crate::trackers::now(),"tcp_loopback":checks,
        "utp_listener_bound":diagnostics["utp_enabled"],"public_reachability":"unverified"})
}

pub fn metadata_stage(trace: &Value) -> &'static str {
    if trace["completed"].as_u64().unwrap_or(0) > 0 {
        "元数据已验证"
    } else if trace["collected_pieces"].as_u64().unwrap_or(0) > 0 {
        "已保留部分元数据，继续寻找缺失片段"
    } else if trace["peers"]
        .as_array()
        .is_some_and(|peers| peers.iter().any(|p| p["state"] == "receiving_metadata"))
    {
        "正在接收元数据"
    } else if trace["peers"]
        .as_array()
        .is_some_and(|peers| peers.iter().any(|p| p["state"] == "waiting_metadata"))
    {
        "握手已完成，等待元数据"
    } else if trace["peers"].as_array().is_some_and(|peers| {
        peers
            .iter()
            .any(|p| matches!(p["state"].as_str(), Some("connecting" | "handshaking")))
    }) {
        "已发现节点，正在尝试连接和握手"
    } else if trace["errors"].as_u64().unwrap_or(0) > 0
        && trace["peers"].as_array().is_some_and(|peers| {
            !peers.is_empty()
                && peers
                    .iter()
                    .all(|peer| matches!(peer["state"].as_str(), Some("failed" | "cancelled")))
        })
    {
        "已尝试的节点未提供元数据，继续寻找来源"
    } else if trace["metadata_bytes"].as_u64().unwrap_or(0) > 0 {
        "正在接收元数据"
    } else if trace["handshakes"].as_u64().unwrap_or(0) > 0 {
        "握手已完成，等待元数据"
    } else if trace["attempts"].as_u64().unwrap_or(0) > 0 {
        "已发现节点，正在尝试连接和握手"
    } else {
        "尚未发现可尝试的节点"
    }
}

pub fn metadata_failure(error: &str) -> &'static str {
    if error.contains("checksum invalid") {
        "元数据哈希不匹配，已丢弃片段"
    } else if error.contains("rejected remaining metadata") {
        "节点拒绝提供所需元数据"
    } else if error.contains("does not support ut_metadata")
        || error.contains("does not support extended")
    {
        "节点不支持元数据交换"
    } else if error.contains("metadata size")
        || error.contains("UtMetadata")
        || error.contains("metadata piece")
    {
        "元数据大小或片段不合法"
    } else if error.contains("memory budget") {
        "元数据缓存已达上限，等待重试"
    } else if error.contains("timed out") || error.contains("timeout") || error.contains("elapsed")
    {
        "连接或元数据响应超时"
    } else if error.contains("disconnected") {
        "节点已断开连接"
    } else if error.contains("refused") || error.contains("10061") {
        "节点拒绝连接"
    } else if error.is_empty() {
        ""
    } else {
        "连接或元数据交换失败"
    }
}

pub fn ui(ui: &mut eframe::egui::Ui, network: &Value) {
    use crate::i18n::t;
    ui.strong(t("网络与找源诊断"));
    if network["restart_required"] == true {
        ui.colored_label(
            eframe::egui::Color32::from_rgb(190, 125, 35),
            t("网络设置已修改，完全退出并重启后生效"),
        );
    }
    eframe::egui::Grid::new(ui.id().with("network_runtime"))
        .num_columns(2)
        .show(ui, |ui| {
            for (label, value) in [
                (
                    "实际监听地址",
                    network["listen_addr"].as_str().unwrap_or("—").to_owned(),
                ),
                (
                    "地址族",
                    t(if network["ipv4_only"] == true {
                        "仅 IPv4"
                    } else {
                        "IPv4 / IPv6 双栈"
                    }),
                ),
                (
                    "实际传输协议",
                    match (
                        network["tcp_enabled"] == true,
                        network["utp_enabled"] == true,
                    ) {
                        (true, true) => "TCP + uTP",
                        (false, true) => "uTP",
                        (true, false) => "TCP",
                        _ => "—",
                    }
                    .into(),
                ),
                (
                    "DHT",
                    t(if network["dht_enabled"] == true {
                        "已启用"
                    } else {
                        "关闭"
                    }),
                ),
                (
                    "UPnP 端口映射",
                    t(match network["upnp"]["state"].as_str() {
                        Some("disabled") => "关闭",
                        Some("failed") => "映射服务失败",
                        Some("ended") => "映射服务已结束",
                        _ => "正在请求映射，结果尚未验证",
                    }),
                ),
                ("公网入站可达性", t("未验证；监听成功不代表公网可达")),
            ] {
                ui.weak(t(label));
                ui.label(value);
                ui.end_row();
            }
        });
    for key in ["startup_warnings", "warnings"] {
        if let Some(warnings) = network[key].as_array() {
            for warning in warnings {
                if let Some(w) = warning.as_str() {
                    ui.colored_label(eframe::egui::Color32::from_rgb(190, 125, 35), t(w));
                }
            }
        }
    }
    if let Some(error) = network["upnp"]["error"].as_str() {
        ui.weak(error);
    }
    if let Some(checks) = network["local_check"]["tcp_loopback"].as_array() {
        ui.strong(t("本机监听检查"));
        for check in checks {
            ui.horizontal(|ui| {
                ui.label(check["address"].as_str().unwrap_or("—"));
                ui.label(t(if check["ok"] == true {
                    "通过"
                } else {
                    "失败"
                }));
                if let Some(error) = check["error"].as_str() {
                    ui.weak(error);
                }
            });
        }
    }
    ui.weak(t(
        "会话传输连接统计：连接成功不等于 BT 握手成功，不等于已传输文件。",
    ));
    eframe::egui::Grid::new(ui.id().with("network_connections"))
        .num_columns(4)
        .show(ui, |ui| {
            for label in ["协议 / 地址族", "连接尝试", "连接建立", "连接错误"] {
                ui.strong(t(label));
            }
            ui.end_row();
            for transport in ["tcp", "utp"] {
                for family in ["v4", "v6"] {
                    let stats = &network["connections"][transport][family];
                    ui.label(format!("{} / {}", transport.to_uppercase(), family));
                    for key in ["attempts", "successes", "errors"] {
                        ui.label(stats[key].as_u64().unwrap_or(0).to_string());
                    }
                    ui.end_row();
                }
            }
        });
}

/// Explicit diagnostic command: list-only metadata requests never create payload files.
pub async fn probe(input: &std::path::Path, output: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    let bytes = std::fs::read(input)?;
    anyhow::ensure!(bytes.len() <= 1_000_000, "诊断输入过大");
    let input: Value = serde_json::from_slice(&bytes)?;
    let magnets = input["magnets"].as_array().context("缺少 magnets 数组")?;
    anyhow::ensure!(
        !magnets.is_empty() && magnets.len() <= 8,
        "诊断磁力数量必须为 1–8"
    );
    let root = output
        .parent()
        .context("诊断输出需要绝对路径")?
        .join(format!("probe-data-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("data"))?;
    let mut settings = Settings::defaults(&root);
    settings.listen_port = 0;
    settings.upnp_enabled = false;
    for (key, setting) in [
        ("tcp_enabled", &mut settings.tcp_enabled),
        ("utp_enabled", &mut settings.utp_enabled),
        ("ipv6_enabled", &mut settings.ipv6_enabled),
    ] {
        if let Some(value) = input.get(key) {
            *setting = value.as_bool().context("诊断网络开关必须为布尔值")?;
        }
    }
    settings.validate()?;
    std::fs::write(
        root.join("data/settings.json"),
        serde_json::to_vec(&settings)?,
    )?;
    let engine = crate::engine::Engine::new(&root, false).await?;
    let mut jobs = tokio::task::JoinSet::new();
    let mut hashes = Vec::new();
    let trackers = input["trackers"].as_array().cloned().unwrap_or_default();
    for value in magnets {
        let source = value.as_str().context("磁力必须为字符串")?;
        let parsed = librqbit::Magnet::parse(source)?;
        let hash = parsed.as_id20().context("诊断需要 v1 磁力哈希")?;
        hashes.push(hash);
        let mut url = url::Url::parse(source)?;
        if parsed.trackers.is_empty() {
            for tracker in trackers.iter().filter_map(Value::as_str).take(60) {
                if let Some(normalized) = crate::trackers::normalize(tracker) {
                    url.query_pairs_mut().append_pair("tr", &normalized);
                }
            }
        }
        let session = engine.session.clone();
        let label = hash.as_string();
        jobs.spawn(async move {
            let result=tokio::time::timeout(std::time::Duration::from_secs(90),session.add_torrent(
                librqbit::AddTorrent::from_url(url.to_string()),Some(librqbit::AddTorrentOptions{list_only:true,..Default::default()})
            )).await;
            match result {
                Ok(Ok(librqbit::AddTorrentResponse::ListOnly(response)))=>json!({"hash":label,"metadata_ready":true,"metadata_size":response.torrent_bytes.len()}),
                Ok(Ok(_))=>json!({"hash":label,"error":"unexpected managed task response"}),
                Ok(Err(error))=>json!({"hash":label,"metadata_ready":false,"error":format!("{error:#}")}),
                Err(_)=>json!({"hash":label,"metadata_ready":false,"error":"90 秒内未收到完整元数据"}),
            }
        });
    }
    let mut results = Vec::new();
    let started = crate::trackers::now();
    let write_report = |results: &Vec<Value>, running: bool| -> anyhow::Result<()> {
        let traces=hashes.iter().map(|hash|json!({"hash":hash.as_string(),"trace":engine.session.resolution_diagnostics(*hash)})).collect::<Vec<_>>();
        let report = json!({"running":running,"started":started,"updated":crate::trackers::now(),"network":engine.session.network_diagnostics(),"results":results,"observations":traces,"payload_downloaded":false});
        let staged = output.with_extension("tmp");
        std::fs::write(&staged, serde_json::to_vec_pretty(&report)?)?;
        std::fs::rename(staged, output)?;
        Ok(())
    };
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
    while !jobs.is_empty() {
        tokio::select! {
            done=jobs.join_next()=>{if let Some(result)=done {results.push(result?);}},
            _=interval.tick()=>{write_report(&results,true)?;},
        }
    }
    write_report(&results, false)?;
    engine.session.stop().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_diagnostics_describe_current_work_and_actual_failures() {
        assert_eq!(
            metadata_stage(&json!({"handshakes":10,"peers":[{"state":"connecting"}]})),
            "已发现节点，正在尝试连接和握手"
        );
        assert_eq!(
            metadata_stage(&json!({"collected_pieces":1,"errors":2,"peers":[{"state":"failed"}]})),
            "已保留部分元数据，继续寻找缺失片段"
        );
        assert_eq!(
            metadata_failure("peer rejected remaining metadata requests"),
            "节点拒绝提供所需元数据"
        );
        assert_eq!(
            metadata_failure("info checksum invalid; discarded unverified fragments"),
            "元数据哈希不匹配，已丢弃片段"
        );
        assert_eq!(
            metadata_failure("metadata memory budget exhausted"),
            "元数据缓存已达上限，等待重试"
        );
    }
    #[test]
    fn metadata_stages_require_observed_evidence() {
        for (stats, label) in [
            (json!({}), "尚未发现可尝试的节点"),
            (json!({"attempts":2}), "已发现节点，正在尝试连接和握手"),
            (json!({"handshakes":1}), "握手已完成，等待元数据"),
            (json!({"metadata_bytes":100}), "正在接收元数据"),
            (json!({"completed":1}), "元数据已验证"),
        ] {
            assert_eq!(metadata_stage(&stats), label);
        }
        assert_eq!(
            metadata_stage(&json!({"handshakes":1,"errors":1,
            "peers":[{"state":"failed"}]})),
            "已尝试的节点未提供元数据，继续寻找来源"
        );
    }
    #[tokio::test]
    async fn occupied_fixed_port_falls_back_and_local_check_is_honest() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let requested = held.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let session = librqbit::Session::new_with_opts(
            dir.path().into(),
            librqbit::SessionOptions {
                dht: None,
                disable_trackers: true,
                disable_local_service_discovery: true,
                ipv4_only: true,
                listen: Some(librqbit::ListenerOptions {
                    listen_addr: ([127, 0, 0, 1], requested).into(),
                    ipv4_only: true,
                    allow_port_fallback: true,
                    mode: librqbit::ListenerMode::TcpAndUtp,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_ne!(session.listen_addr().unwrap().port(), requested);
        assert!(
            !session.network_diagnostics()["warnings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let check = check_local(&session).await;
        assert_eq!(check["tcp_loopback"][0]["ok"], true);
        assert_eq!(check["public_reachability"], "unverified");
        session.stop().await;
    }
    #[tokio::test]
    async fn failed_udp_binding_does_not_claim_utp_is_enabled() {
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = librqbit::Session::new_with_opts(
            dir.path().into(),
            librqbit::SessionOptions {
                dht: None,
                disable_trackers: true,
                disable_local_service_discovery: true,
                ipv4_only: true,
                listen: Some(librqbit::ListenerOptions {
                    listen_addr: held.local_addr().unwrap(),
                    ipv4_only: true,
                    mode: librqbit::ListenerMode::TcpAndUtp,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let diagnostics = session.network_diagnostics();
        assert_eq!(diagnostics["tcp_enabled"], true);
        assert_eq!(diagnostics["utp_enabled"], false);
        assert!(!diagnostics["warnings"].as_array().unwrap().is_empty());
        session.stop().await;
    }
}
