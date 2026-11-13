// Modified in hyperdesk from upstream bahmgit/hlcli (Apache-2.0): removed HTTP server test and added R-01, R-03, R-07, R-09, and R-10 boundary checks.
mod support;

use std::{collections::BTreeMap, fs, path::PathBuf};

use alloy_primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;
use hl_v2::{
    config::Config,
    exchange::Network,
    ipc,
    protocol::{
        Action, BatchOrder, Chain, Cloid, OrderGrouping, OrderRequest, OrderType, TimeInForce,
    },
    security::{
        Credentials, load_credentials, read_password_file, resolve_kdf_params, store_credentials,
        store_validated_credentials, validate_agent_signer,
    },
};

use support::{dec, now_ms};

fn temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hl-v2-{label}-{}-{}", std::process::id(), now_ms()))
}

fn signing_action() -> Action {
    Action::Order(BatchOrder {
        grouping: OrderGrouping::Na,
        orders: vec![OrderRequest {
            asset: 0,
            is_buy: true,
            price: dec("50000.12"),
            size: dec("0.01"),
            reduce_only: false,
            order_type: OrderType::Limit {
                tif: TimeInForce::Gtc,
            },
            cloid: Cloid::from_uuid_text("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
        }],
    })
}

#[test]
fn l1_hash_signature_and_envelope_remain_wire_stable() {
    let action = signing_action();
    let nonce = 1_700_000_000_123_u64;
    let expires_after = 1_700_000_010_123_u64;
    let vault: Address = "0x1111111111111111111111111111111111111111"
        .parse()
        .unwrap();
    assert_eq!(
        action
            .hash(nonce, Some(vault), Some(expires_after))
            .unwrap()
            .to_string(),
        "0x2302ed30802276d1a804896cd67395c2561ee9f53d6822fd363a9e5337a697d1"
    );
    // Public deterministic fixture, never an account key.
    let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x01)).unwrap();
    let signed = action
        .sign(
            &signer,
            nonce,
            Some(vault),
            Some(expires_after),
            Chain::Mainnet,
        )
        .unwrap();
    assert_eq!(
        signed.signature.to_string(),
        "0xf68911f9c4f24e0e3ec2e87d911f7563fb4fd58268503d51b1ae5c59a13097c9249cb66ad992e3af8d341fff23b60572dbaa2281858d9626aed149fcf6a42fe21b"
    );
    assert_eq!(signed.vault_address, Some(vault));
    assert_eq!(signed.expires_after, Some(expires_after));
}

#[test]
fn config_is_strict_and_remote_bind_requires_both_opt_ins() {
    // R-01: Config::load rejects removed HTTP keys (bind, allowRemote, backendToken) and unknown keys via deny_unknown_fields.
    let dir = temp_dir("config");
    fs::create_dir_all(&dir).unwrap();
    for rejected_json in [
        r#"{"network":"Mainnet","bind":"127.0.0.1:8088"}"#,
        r#"{"network":"Mainnet","allowRemote":true}"#,
        r#"{"network":"Mainnet","backendToken":"secret"}"#,
        r#"{"network":"Mainnet","unknownField":123}"#,
    ] {
        fs::write(dir.join("hl-v2.json"), rejected_json).unwrap();
        assert!(
            Config::load(Some(dir.clone())).is_err(),
            "expected Config::load to reject {rejected_json}"
        );
    }

    fs::write(
        dir.join("hl-v2.json"),
        r#"{
          "network":"Mainnet", "defaultSymbol":"btc", "allowedSymbols":["btc"],
          "symbolAliases":{"x":"btc"}
        }"#,
    )
    .unwrap();
    let config = Config::load(Some(dir.clone())).unwrap();
    assert_eq!(config.network, Network::Mainnet);
    assert_eq!(
        config.symbol_aliases,
        BTreeMap::from([("X".to_string(), "BTC".to_string())])
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn credential_profile_round_trip_is_encrypted_and_permission_restricted() {
    let dir = temp_dir("credentials");
    let credentials = Credentials {
        main_wallet: "0x1111111111111111111111111111111111111111".to_string(),
        api_private_key: "private-key-material".to_string(),
        network: "Mainnet".to_string(),
    };
    store_credentials(&dir, "Trader_A", b"correct password", &credentials).unwrap();

    let profile_dir = dir.join("wallets/trader_a");
    let blob = fs::read(profile_dir.join("credentials.bin")).unwrap();
    assert!(
        !blob
            .windows(credentials.api_private_key.len())
            .any(|window| { window == credentials.api_private_key.as_bytes() })
    );
    assert!(load_credentials(&dir, Some("trader_a"), b"wrong password").is_err());
    let loaded = load_credentials(&dir, None, b"correct password").unwrap();
    assert_eq!(loaded.main_wallet, credentials.main_wallet);
    assert_eq!(loaded.api_private_key, credentials.api_private_key);
    assert_eq!(loaded.network, credentials.network);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(profile_dir.join("credentials.bin"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn r07_signer_collision_rejected_before_writing_credentials() {
    let dir = temp_dir("r07-signer-collision");
    let master_hex = format!("0x{}", "ab".repeat(32));
    let agent_hex = format!("0x{}", "cd".repeat(32));
    let master_signer: PrivateKeySigner = master_hex.parse().unwrap();
    let agent_signer: PrivateKeySigner = agent_hex.parse().unwrap();
    let master_checksum = master_signer.address().to_checksum(None);
    let master_lower = format!("{:#x}", master_signer.address());

    // Direct validator rejects master and vault collisions case-insensitively.
    assert!(validate_agent_signer(master_signer.address(), &master_lower, None).is_err());
    assert!(validate_agent_signer(master_signer.address(), &master_checksum, None).is_err());
    assert!(
        validate_agent_signer(
            master_signer.address(),
            "0x1111111111111111111111111111111111111111",
            Some(&master_lower),
        )
        .is_err()
    );

    // Persisting master key as API key fails before creating any keystore files.
    let colliding_master = Credentials {
        main_wallet: master_lower.clone(),
        api_private_key: master_hex.clone(),
        network: "Testnet".to_string(),
    };
    assert!(
        store_validated_credentials(&dir, "default", b"strong-pass", &colliding_master, None)
            .is_err()
    );
    assert!(!dir.join("wallets/default/credentials.bin").exists());
    assert!(!dir.join("wallets/default/salt.bin").exists());
    assert!(!dir.join("credentials.bin").exists());
    assert!(!dir.join("wallet_profiles.json").exists());

    // Persisting vault key as API key also fails before creating any keystore files.
    let colliding_vault = Credentials {
        main_wallet: "0x1111111111111111111111111111111111111111".to_string(),
        api_private_key: master_hex,
        network: "Testnet".to_string(),
    };
    assert!(
        store_validated_credentials(
            &dir,
            "default",
            b"strong-pass",
            &colliding_vault,
            Some(&master_checksum),
        )
        .is_err()
    );
    assert!(!dir.join("wallets/default/credentials.bin").exists());
    assert!(!dir.join("credentials.bin").exists());

    // Distinct delegated Agent Wallet succeeds and writes encrypted profile credentials.
    assert!(
        validate_agent_signer(
            agent_signer.address(),
            &master_checksum,
            Some("0x2222222222222222222222222222222222222222"),
        )
        .is_ok()
    );
    let distinct_agent = Credentials {
        main_wallet: master_checksum,
        api_private_key: agent_hex,
        network: "Testnet".to_string(),
    };
    store_validated_credentials(&dir, "default", b"strong-pass", &distinct_agent, None).unwrap();
    assert!(dir.join("wallets/default/credentials.bin").exists());
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[tokio::test]
async fn r09_ipc_bind_enforces_private_parent_dir_and_peer_uid() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = temp_dir("r09-ipc");
    fs::create_dir_all(&root).unwrap();

    // Group/world-writable parent directory is rejected before binding.
    let open_parent = root.join("world-open");
    fs::create_dir_all(&open_parent).unwrap();
    fs::set_permissions(&open_parent, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ipc::bind(&open_parent.join("backend.sock")).await.is_err());
    assert!(!open_parent.join("backend.sock").exists());

    // Symlink parent directory is rejected before binding.
    let real_private = root.join("real-private");
    fs::create_dir_all(&real_private).unwrap();
    fs::set_permissions(&real_private, fs::Permissions::from_mode(0o700)).unwrap();
    let link_parent = root.join("symlink-parent");
    symlink(&real_private, &link_parent).unwrap();
    assert!(ipc::bind(&link_parent.join("backend.sock")).await.is_err());
    assert!(!real_private.join("backend.sock").exists());

    // Missing private directory is created with 0700 and socket with 0600.
    let auto_parent = root.join("auto-runtime");
    let sock_path = auto_parent.join("backend.sock");
    let listener = ipc::bind(&sock_path).await.unwrap();
    assert_eq!(
        fs::symlink_metadata(&auto_parent)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::symlink_metadata(&sock_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(listener);

    // Peer UID validator accepts current euid and rejects foreign UIDs.
    let euid = ipc::current_euid();
    assert!(ipc::check_peer_uid(euid, euid).is_ok());
    assert!(ipc::check_peer_uid(euid.wrapping_add(1), euid).is_err());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn r10_password_file_kdf_floors_and_debug_redaction_enforced() {
    // Debug formatting never leaks api_private_key.
    let secret = "cd".repeat(32);
    let creds = Credentials {
        main_wallet: "0x1111111111111111111111111111111111111111".to_string(),
        api_private_key: secret.clone(),
        network: "Mainnet".to_string(),
    };
    let debug_text = format!("{creds:?}");
    assert!(debug_text.contains("[REDACTED]"));
    assert!(!debug_text.contains(&secret));

    // Pure KDF parameter validator enforces floors unconditionally.
    let defaults = resolve_kdf_params(None, None).unwrap();
    assert_eq!(defaults.m_cost(), 19_456);
    assert_eq!(defaults.t_cost(), 2);
    assert_eq!(defaults.p_cost(), 1);

    let stronger = resolve_kdf_params(Some("32768"), Some("3")).unwrap();
    assert_eq!(stronger.m_cost(), 32_768);
    assert_eq!(stronger.t_cost(), 3);

    for (mem, iters) in [
        (Some("19455"), None),
        (Some("0"), None),
        (Some("not-a-number"), None),
        (None, Some("1")),
        (None, Some("0")),
        (None, Some("bad")),
    ] {
        assert!(
            resolve_kdf_params(mem, iters).is_err(),
            "expected KDF floor rejection for mem={mem:?}, iters={iters:?}"
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = temp_dir("r10-passfile");
        fs::create_dir_all(&dir).unwrap();
        let pass_path = dir.join("pass.txt");
        fs::write(&pass_path, b"  spaced passphrase  \r\n").unwrap();

        for insecure_mode in [0o644, 0o640, 0o604] {
            fs::set_permissions(&pass_path, fs::Permissions::from_mode(insecure_mode)).unwrap();
            assert!(
                read_password_file(&pass_path).is_err(),
                "expected rejection for mode {insecure_mode:04o}"
            );
        }

        fs::set_permissions(&pass_path, fs::Permissions::from_mode(0o600)).unwrap();
        let symlink_path = dir.join("pass-link.txt");
        symlink(&pass_path, &symlink_path).unwrap();
        assert!(read_password_file(&symlink_path).is_err());

        let loaded = read_password_file(&pass_path).unwrap();
        assert_eq!(loaded.as_str(), "  spaced passphrase  ");

        fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn r03_include_hip3_defaults_true_and_no_hip3_guards_startup_and_live_exposure() {
    use futures_util::{SinkExt, StreamExt};
    use hl_v2::{
        execution::ExecutionControl,
        feed::apply_ws_message_for,
        info::InfoClient,
        ipc::{IpcApp, IpcRequest, IpcResponse},
        metrics::Metrics,
        runtime::{CommandQueue, CommandService, ReadOnlyExecutor},
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeSet, sync::Arc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::RwLock,
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    let dir = temp_dir("r03-hip3");
    fs::create_dir_all(&dir).unwrap();

    // (1) Default Config enables full market access (`include_hip3 == true`, `allowed_symbols == []`).
    let defaults = Config::defaults(dir.clone());
    assert!(defaults.include_hip3);
    assert!(defaults.allowed_symbols.is_empty());

    // (2) `includeHip3 = false` rejects `dex:COIN` in `defaultSymbol` or `allowedSymbols` and accepts native symbols.
    for rejected_json in [
        r#"{"network":"Testnet","includeHip3":false,"defaultSymbol":"xyz:TSLA"}"#,
        r#"{"network":"Testnet","includeHip3":false,"defaultSymbol":"BTC","allowedSymbols":["BTC","xyz:TSLA"]}"#,
    ] {
        fs::write(dir.join("hl-v2.json"), rejected_json).unwrap();
        let err = Config::load(Some(dir.clone())).unwrap_err().to_string();
        assert!(
            err.contains("includeHip3"),
            "expected HIP-3 config error for {rejected_json}, got: {err}"
        );
    }
    fs::write(
        dir.join("hl-v2.json"),
        r#"{"network":"Testnet","includeHip3":false,"defaultSymbol":"BTC"}"#,
    )
    .unwrap();
    let valid_no_hip3 = Config::load(Some(dir.clone())).unwrap();
    assert!(!valid_no_hip3.include_hip3);

    async fn spawn_hip3_mock_exchange(
        user: String,
        all_dexs_states: Value,
        xyz_open_orders: Value,
    ) -> (
        String,
        String,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info_url = format!("http://{}/info", http_listener.local_addr().unwrap());
        let http_handle = tokio::spawn(async move {
            while let Ok((mut sock, _)) = http_listener.accept().await {
                let xyz_open_orders = xyz_open_orders.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let header_end = loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..header_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let req: Value =
                        serde_json::from_slice(&buf[header_end..header_end + content_len]).unwrap();
                    let resp = match req.get("type").and_then(Value::as_str) {
                        Some("perpDexs") => json!([null, {"name": "xyz"}]),
                        Some("allPerpMetas") => json!([
                            {"universe": [{"name": "BTC", "szDecimals": 5, "maxLeverage": 40}]},
                            {"universe": [{"name": "xyz:TSLA", "szDecimals": 2, "maxLeverage": 10}]}
                        ]),
                        Some("spotMetaAndAssetCtxs") => json!([
                            {
                                "tokens": [
                                    {"name": "USDC", "szDecimals": 8, "index": 0},
                                    {"name": "PURR", "szDecimals": 0, "index": 1}
                                ],
                                "universe": [
                                    {"name": "PURR/USDC", "index": 0, "tokens": [1, 0]}
                                ]
                            },
                            []
                        ]),
                        Some("l2Book") => json!({
                            "levels": [
                                [{"px": "50000.0", "sz": "1.0", "n": 1}],
                                [{"px": "50001.0", "sz": "1.0", "n": 1}]
                            ]
                        }),
                        Some("clearinghouseState") => json!({
                            "marginSummary": {
                                "accountValue": "1000.0",
                                "totalMarginUsed": "0.0",
                                "totalNtlPos": "0.0"
                            },
                            "withdrawable": "1000.0",
                            "assetPositions": []
                        }),
                        Some("frontendOpenOrders") => {
                            if req.get("dex").and_then(Value::as_str) == Some("xyz") {
                                xyz_open_orders
                            } else {
                                json!([])
                            }
                        }
                        Some("userAbstraction") => json!("standard"),
                        Some("spotClearinghouseState") => json!({
                            "balances": [
                                {"coin": "USDC", "token": 0, "total": "500.0", "hold": "0.0", "entryNtl": "500.0"}
                            ]
                        }),
                        Some("allMids") => json!({"PURR/USDC": "1.25", "BTC": "50000.5"}),
                        Some("borrowLendUserState") => {
                            json!({"tokenToState": [], "health": "healthy"})
                        }
                        Some("activeAssetData") => json!({
                            "leverage": {"type": "cross", "value": 10},
                            "maxTradeSzs": ["10.0", "10.0"],
                            "availableToTrade": ["1000.0", "1000.0"],
                            "markPx": "50000.5"
                        }),
                        other => panic!("unexpected /info request type {other:?}"),
                    };
                    let body = serde_json::to_vec(&resp).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                });
            }
        });

        let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}", ws_listener.local_addr().unwrap());
        let ws_handle = tokio::spawn(async move {
            while let Ok((stream, _)) = ws_listener.accept().await {
                let user = user.clone();
                let all_dexs_states = all_dexs_states.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = accept_async(stream).await else {
                        return;
                    };
                    while let Some(Ok(msg)) = ws.next().await {
                        match msg {
                            Message::Text(text) => {
                                let Ok(req) = serde_json::from_str::<Value>(text.as_ref()) else {
                                    continue;
                                };
                                match req.pointer("/subscription/type").and_then(Value::as_str) {
                                    Some("openOrders") => {
                                        let dex = req
                                            .pointer("/subscription/dex")
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        let reply = json!({
                                            "channel": "openOrders",
                                            "data": {
                                                "user": user,
                                                "dex": dex,
                                                "orders": []
                                            }
                                        });
                                        let _ =
                                            ws.send(Message::Text(reply.to_string().into())).await;
                                    }
                                    Some("allDexsClearinghouseState") => {
                                        let reply = json!({
                                            "channel": "allDexsClearinghouseState",
                                            "data": {
                                                "user": user,
                                                "clearinghouseStates": all_dexs_states
                                            }
                                        });
                                        let _ =
                                            ws.send(Message::Text(reply.to_string().into())).await;
                                    }
                                    _ => {}
                                }
                            }
                            Message::Close(_) => {
                                let _ = ws.close(None).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        (info_url, ws_url, http_handle, ws_handle)
    }

    let user = "0x1111111111111111111111111111111111111111".to_string();

    // Default `seed_state` (`include_hip3 == true`) loads native Perp (`BTC`), Spot (`SPOT:PURR/USDC`), and HIP-3 (`XYZ:TSLA`).
    {
        let (info_url, ws_url, http_h, ws_h) = spawn_hip3_mock_exchange(
            user.clone(),
            json!([["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}]]),
            json!([]),
        )
        .await;
        let client = InfoClient::new(Network::Testnet)
            .with_info_url(info_url)
            .with_ws_url(ws_url);
        let seeded = client
            .seed_state("BTC", &BTreeSet::new(), Some(&user))
            .await
            .unwrap();
        assert!(seeded.include_hip3);
        assert!(seeded.markets.contains_key("BTC"));
        assert!(seeded.markets.contains_key("SPOT:PURR/USDC"));
        assert!(seeded.markets.contains_key("XYZ:TSLA"));
        http_h.abort();
        ws_h.abort();
    }

    // `include_hip3 == false` succeeds when non-root DEXes have zero positions/margin/orders and excludes `XYZ:TSLA`.
    {
        let (info_url, ws_url, http_h, ws_h) = spawn_hip3_mock_exchange(
            user.clone(),
            json!([
                ["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}],
                ["xyz", {"assetPositions": [], "marginSummary": {"accountValue": "0.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "0.0"}]
            ]),
            json!([]),
        )
        .await;
        let client = InfoClient::new(Network::Testnet)
            .with_info_url(info_url)
            .with_ws_url(ws_url);
        let seeded = client
            .seed_state_with_options("BTC", &BTreeSet::new(), Some(&user), false)
            .await
            .unwrap();
        assert!(!seeded.include_hip3);
        assert!(seeded.markets.contains_key("BTC"));
        assert!(seeded.markets.contains_key("SPOT:PURR/USDC"));
        assert!(!seeded.markets.contains_key("XYZ:TSLA"));
        http_h.abort();
        ws_h.abort();
    }

    // (3) + Sensitivity Hint #3: `include_hip3 == false` fails closed naming `"xyz"` when `allDexsClearinghouseState`
    // reports non-zero `szi` (`-1.5`), `totalMarginUsed > 0`, or open orders on DEX `"xyz"`.
    {
        let (info_url, ws_url, http_h, ws_h) = spawn_hip3_mock_exchange(
            user.clone(),
            json!([
                ["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}],
                ["xyz", {
                    "assetPositions": [
                        {"position": {"coin": "xyz:TSLA", "szi": "-1.5"}}
                    ],
                    "marginSummary": {"accountValue": "250.0", "totalMarginUsed": "0.0", "totalNtlPos": "300.0"},
                    "withdrawable": "250.0"
                }]
            ]),
            json!([]),
        )
        .await;
        let client = InfoClient::new(Network::Testnet)
            .with_info_url(info_url)
            .with_ws_url(ws_url);
        let err = client
            .seed_state_with_options("BTC", &BTreeSet::new(), Some(&user), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("xyz"),
            "expected startup error naming xyz for short exposure, got: {err}"
        );
        http_h.abort();
        ws_h.abort();
    }

    {
        let (info_url, ws_url, http_h, ws_h) = spawn_hip3_mock_exchange(
            user.clone(),
            json!([
                ["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}],
                ["xyz", {
                    "assetPositions": [],
                    "marginSummary": {"accountValue": "250.0", "totalMarginUsed": "12.5", "totalNtlPos": "0.0"},
                    "withdrawable": "237.5"
                }]
            ]),
            json!([]),
        )
        .await;
        let client = InfoClient::new(Network::Testnet)
            .with_info_url(info_url)
            .with_ws_url(ws_url);
        let err = client
            .seed_state_with_options("BTC", &BTreeSet::new(), Some(&user), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("xyz"),
            "expected startup error naming xyz for totalMarginUsed > 0, got: {err}"
        );
        http_h.abort();
        ws_h.abort();
    }

    {
        let (info_url, ws_url, http_h, ws_h) = spawn_hip3_mock_exchange(
            user.clone(),
            json!([
                ["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}],
                ["xyz", {
                    "assetPositions": [],
                    "marginSummary": {"accountValue": "250.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"},
                    "withdrawable": "250.0"
                }]
            ]),
            json!([{"oid": 991_u64, "coin": "xyz:TSLA", "side": "B", "limitPx": "180.0", "sz": "1.0"}]),
        )
        .await;
        let client = InfoClient::new(Network::Testnet)
            .with_info_url(info_url)
            .with_ws_url(ws_url);
        let err = client
            .seed_state_with_options("BTC", &BTreeSet::new(), Some(&user), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("xyz"),
            "expected startup error naming xyz for open order, got: {err}"
        );
        http_h.abort();
        ws_h.abort();
    }

    // (4) Live WS `allDexsClearinghouseState` or `userFills` with `include_hip3 == false` persistently degrades health (`HealthPayload.ok = false`) and `doctor`.
    {
        let mut base = support::ready_perp("0");
        base.include_hip3 = false;
        let state = Arc::new(RwLock::new(base));
        let metrics = Metrics::default();
        metrics.state_ws_connected();
        let execution = ExecutionControl::default();
        let service = Arc::new(
            CommandService::new(state.clone(), Arc::new(ReadOnlyExecutor)).with_diagnostics(
                metrics.clone(),
                execution.clone(),
                None,
            ),
        );
        let app = IpcApp::new(
            state.clone(),
            metrics,
            CommandQueue::new(service.clone()).unwrap(),
        )
        .with_execution_control(execution);

        match app.dispatch(IpcRequest::Health).await {
            IpcResponse::Health(payload) => assert!(payload.ok),
            other => panic!("unexpected response: {other:?}"),
        }

        let watched = BTreeSet::from(["BTC".to_string()]);
        let live_ch_msg = json!({
            "channel": "allDexsClearinghouseState",
            "data": {
                "user": user,
                "clearinghouseStates": [
                    ["", {"assetPositions": [], "marginSummary": {"accountValue": "1000.0", "totalMarginUsed": "0.0", "totalNtlPos": "0.0"}, "withdrawable": "1000.0"}],
                    ["xyz", {"assetPositions": [{"position": {"coin": "xyz:TSLA", "szi": "2.0"}}], "marginSummary": {"accountValue": "500.0", "totalMarginUsed": "50.0", "totalNtlPos": "400.0"}, "withdrawable": "450.0"}]
                ]
            }
        })
        .to_string();
        apply_ws_message_for(&state, &live_ch_msg, Some(&user), &watched)
            .await
            .unwrap();

        match app.dispatch(IpcRequest::Health).await {
            IpcResponse::Health(payload) => {
                assert!(!payload.ok);
                assert!(payload.reasons.iter().any(|r| r.contains("xyz")));
            }
            other => panic!("unexpected response: {other:?}"),
        }
        let doctor = service.execute_line("doctor").await;
        assert!(!doctor.ok);
        assert!(doctor.lines.iter().any(|line| line.contains("xyz")));

        // Verify live `userFills` for `dex:COIN` also triggers the persistent health guard on a clean `--no-hip3` state.
        let mut base2 = support::ready_perp("0");
        base2.include_hip3 = false;
        let state2 = Arc::new(RwLock::new(base2));
        let metrics2 = Metrics::default();
        metrics2.state_ws_connected();
        let service2 = Arc::new(CommandService::new(
            state2.clone(),
            Arc::new(ReadOnlyExecutor),
        ));
        let app2 = IpcApp::new(
            state2.clone(),
            metrics2,
            CommandQueue::new(service2).unwrap(),
        );
        let live_fill_msg = json!({
            "channel": "userFills",
            "data": {
                "user": user,
                "fills": [
                    {
                        "coin": "xyz:NVDA",
                        "px": "130.5",
                        "sz": "1.0",
                        "side": "B",
                        "time": now_ms(),
                        "oid": 555_u64,
                        "tid": 777_u64,
                        "closedPnl": "0.0"
                    }
                ]
            }
        })
        .to_string();
        apply_ws_message_for(&state2, &live_fill_msg, Some(&user), &watched)
            .await
            .unwrap();
        match app2.dispatch(IpcRequest::Health).await {
            IpcResponse::Health(payload) => {
                assert!(!payload.ok);
                assert!(payload.reasons.iter().any(|r| r.contains("xyz")));
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fs::remove_dir_all(dir).unwrap();
}
