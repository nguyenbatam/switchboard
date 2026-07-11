use crate::agent::registry::{ToolSpec, WalletToolConfig};
use crate::agent::taint::{Effects, Trust};
use crate::domain::schema::{FieldSpec, ToolParameters};
use crate::domain::values::ParsedToolArguments;
use alloy_primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
struct WalletBalanceArgs {
    address: String,
}

#[derive(Serialize)]
struct WalletBalanceOk {
    status: &'static str,
    address: String,
    balance_wei: String,
    block: u64,
}

#[derive(Serialize)]
struct WalletBalanceStale {
    status: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u32,
    method: &'static str,
    params: &'a [serde_json::Value],
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<String>,
}

pub fn build(cfg: &WalletToolConfig) -> ToolSpec {
    let cfg = Arc::new(cfg.clone());
    ToolSpec::new(
        "wallet_balance",
        "Return the ETH balance of an Ethereum address (decimal string, never float).",
        ToolParameters::object(
            BTreeMap::from([(
                "address".into(),
                FieldSpec::string("Checksummed Ethereum address (0x...)"),
            )]),
            vec!["address".into()],
            false,
        ),
        Trust::Untrusted,
        Effects::Egress,
        cfg.timeout,
        move |_cx, args| run(cfg.clone(), args),
    )
}

fn run(cfg: Arc<WalletToolConfig>, args: &ParsedToolArguments) -> Result<String, String> {
    let args: WalletBalanceArgs = args.typed()?;
    let address_str = args.address;

    let address: Address = address_str
        .parse()
        .map_err(|_| "invalid Ethereum address".to_string())?;
    let checksummed = address.to_checksum(None);
    if address_str != checksummed && !address_str.eq_ignore_ascii_case(&format!("{address:#x}")) {
        return Err(format!("address checksum mismatch; expected {checksummed}"));
    }

    if cfg.rpc_urls.len() < 2 {
        return Err("wallet_balance requires at least two RPC endpoints".into());
    }

    let rt = tokio::runtime::Handle::current();
    let results: Result<Vec<RpcSnapshot>, String> = rt.block_on(async {
        let mut snaps = Vec::new();
        for url in cfg.rpc_urls.iter().take(2) {
            snaps.push(query_rpc_with_retry(url, &address, cfg.timeout).await?);
        }
        Ok(snaps)
    });

    let snaps = results?;
    let drift = snaps[0].block.abs_diff(snaps[1].block);
    if drift > cfg.max_block_drift {
        return Ok(serde_json::to_string(&WalletBalanceStale {
            status: "stale",
            reason: "rpc block height disagreement",
        })
        .unwrap());
    }

    if snaps[0].balance != snaps[1].balance {
        return Ok(serde_json::to_string(&WalletBalanceStale {
            status: "stale",
            reason: "rpc balance disagreement",
        })
        .unwrap());
    }

    Ok(serde_json::to_string(&WalletBalanceOk {
        status: "ok",
        address: address.to_checksum(None),
        balance_wei: u256_to_decimal(snaps[0].balance),
        block: snaps[0].block,
    })
    .unwrap())
}

struct RpcSnapshot {
    balance: U256,
    block: u64,
}

/// One retry. A public RPC endpoint drops requests often enough that a single
/// transient failure should not be reported to the model as "the chain is down",
/// and often enough that a second failure means it really is.
async fn query_rpc_with_retry(
    url: &str,
    address: &Address,
    timeout: Duration,
) -> Result<RpcSnapshot, String> {
    match query_rpc(url, address, timeout).await {
        Ok(snap) => Ok(snap),
        Err(first) => {
            tracing::debug!(url, error = %first, "rpc query failed, retrying once");
            query_rpc(url, address, timeout)
                .await
                .map_err(|second| format!("rpc failed twice: {first}; then: {second}"))
        }
    }
}

async fn query_rpc(url: &str, address: &Address, timeout: Duration) -> Result<RpcSnapshot, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| e.to_string())?;

    let block_req = JsonRpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "eth_blockNumber",
        params: &[],
    };
    let address_param = serde_json::Value::String(format!("{address:#x}"));
    let latest_param = serde_json::Value::String("latest".into());
    let balance_params = [address_param, latest_param];
    let balance_req = JsonRpcRequest {
        jsonrpc: "2.0",
        id: 2,
        method: "eth_getBalance",
        params: &balance_params,
    };

    let block_resp: JsonRpcResponse = client
        .post(url)
        .json(&block_req)
        .send()
        .await
        .map_err(|e| format!("rpc block request failed: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let balance_resp: JsonRpcResponse = client
        .post(url)
        .json(&balance_req)
        .send()
        .await
        .map_err(|e| format!("rpc balance request failed: {e}"))?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let block_hex = block_resp.result.as_deref().ok_or("missing block result")?;
    let balance_hex = balance_resp
        .result
        .as_deref()
        .ok_or("missing balance result")?;

    let block = u64::from_str_radix(block_hex.trim_start_matches("0x"), 16)
        .map_err(|_| "invalid block number".to_string())?;
    let balance = U256::from_str_radix(balance_hex.trim_start_matches("0x"), 16)
        .map_err(|_| "invalid balance".to_string())?;

    Ok(RpcSnapshot { balance, block })
}

fn u256_to_decimal(v: U256) -> String {
    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u256_decimal_no_float() {
        let v = U256::from(1_000_000_000_000_000_000u64);
        assert_eq!(u256_to_decimal(v), "1000000000000000000");
    }
}
