use crate::js_tracer;
use crate::js_tracer::types::{BalanceOverlay, CodeOverlay, StorageOverlay};
use crate::js_tracer::utils::{format_hex_u256, parse_address, parse_b256};
use alloy::primitives::{Address, B256, U256};
use boa_engine::object::FunctionObjectBuilder;
use boa_engine::{
    Context as BoaContext, Context, JsArgs, JsValue, NativeFunction, Source, js_string,
};
use boa_gc::{Finalize, Trace};
use js_tracer::utils::anyhow_error_to_js_error;
use ruint::aliases::B160;
use serde_json::Value as JsonValue;
use std::{cell::RefCell, rc::Rc};
use zk_ee::common_structs::derive_flat_storage_key;
use zk_os_api::helpers::get_code;
use zksync_os_storage_api::ViewState;

#[derive(Trace, Finalize)]
struct HostEnvironment<V: ViewState + 'static> {
    #[unsafe_ignore_trace]
    state_view: RefCell<V>,
    #[unsafe_ignore_trace]
    storage_overlay: Rc<RefCell<StorageOverlay>>,
    #[unsafe_ignore_trace]
    code_overlay: Rc<RefCell<CodeOverlay>>,
    #[unsafe_ignore_trace]
    balance_overlay: Rc<RefCell<BalanceOverlay>>,
}

#[allow(clippy::enum_variant_names)]
enum HostMethod {
    GetBalance,
    GetNonce,
    GetCode,
    GetState,
    Exists,
}

impl HostMethod {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "getBalance" => Some(Self::GetBalance),
            "getNonce" => Some(Self::GetNonce),
            "getCode" => Some(Self::GetCode),
            "getState" => Some(Self::GetState),
            "exists" => Some(Self::Exists),
            _ => None,
        }
    }
}

pub(crate) fn init_host_env_in_boa_context(
    ctx: &mut Context,
    tracer_source: &str,
    state_view: RefCell<impl ViewState + 'static>,
    storage_overlay: Rc<RefCell<StorageOverlay>>,
    code_overlay: Rc<RefCell<CodeOverlay>>,
    balance_overlay: Rc<RefCell<BalanceOverlay>>,
) -> anyhow::Result<()> {
    bootstrap_tracer(ctx, tracer_source)?;

    let host_env = HostEnvironment {
        state_view,
        storage_overlay,
        code_overlay,
        balance_overlay,
    };

    install_host_bindings(ctx, host_env)?;
    install_db_wrapper(ctx)?;
    install_step_helpers(ctx)?;

    Ok(())
}

// A wrapper around the tracer JS code to bootstrap it in the Boa context.
fn bootstrap_tracer(ctx: &mut BoaContext, tracer_source: &str) -> anyhow::Result<()> {
    let bootstrap = format!(
        "var tracer=(function(){{\n
             tracer={tracer_source};\n
             if (typeof tracer === 'object' && tracer) return tracer;\n
             if (typeof exports === 'object' && exports) {{\n
                 var candidate = exports.tracer || exports.default || exports;\n
                 if (typeof candidate === 'object' && candidate) return candidate;\n
             }}\n
             return undefined;}})();",
    );

    ctx.eval(Source::from_bytes(bootstrap.as_bytes()))
        .map_err(|e| anyhow::anyhow!(format!("JS tracer bootstrap error: {e:?}")))?;

    Ok(())
}

fn install_host_bindings<V: ViewState + 'static>(
    ctx: &mut BoaContext,
    env: HostEnvironment<V>,
) -> anyhow::Result<()> {
    let host = FunctionObjectBuilder::new(
        ctx.realm(),
        NativeFunction::from_copy_closure_with_captures(
            |_this, args, env, ctx| {
                let method_name = args
                    .get_or_undefined(0)
                    .to_string(ctx)?
                    .to_std_string_escaped();

                let payload_raw = args
                    .get_or_undefined(1)
                    .to_string(ctx)?
                    .to_std_string_escaped();

                let payload: JsonValue = serde_json::from_str(&payload_raw)
                    .map_err(|err| anyhow_error_to_js_error(anyhow::anyhow!(err)))?;

                let Some(method) = HostMethod::parse(&method_name) else {
                    return Ok(JsValue::from(js_string!("null")));
                };

                let response =
                    dispatch_host_call(env, method, &payload).map_err(anyhow_error_to_js_error)?;

                Ok(JsValue::from(js_string!(response)))
            },
            env,
        ),
    )
    .name(js_string!("__hostCall"))
    .length(2)
    .build();

    ctx.global_object()
        .set(js_string!("__hostCall"), host, false, ctx)
        .map_err(|e| anyhow::anyhow!(format!("install __hostCall failed: {e:?}")))?;

    Ok(())
}

fn install_db_wrapper(ctx: &mut BoaContext) -> anyhow::Result<()> {
    let js_db_wrapper = r#"
        var db = {
            getBalance: function(a){ return __hostCall("getBalance", JSON.stringify({address: a})); },
            getNonce: function(a){ return __hostCall("getNonce", JSON.stringify({address: a})); },
            getCode: function(a){ return __hostCall("getCode", JSON.stringify({address: a})); },
            getState: function(a,s){ return __hostCall("getState", JSON.stringify({address: a, slot: s})); },
            exists: function(a){ return __hostCall("exists", JSON.stringify({address: a})) === 'true'; }
        };
    "#;

    ctx.eval(Source::from_bytes(js_db_wrapper.as_bytes()))
        .map_err(|e| anyhow::anyhow!(format!("install db wrapper failed: {e:?}")))?;

    Ok(())
}

fn install_step_helpers(ctx: &mut BoaContext) -> anyhow::Result<()> {
    let helpers = r#"
            function normalizeHex(value){
                if (typeof value !== 'string') {
                    return '0x';
                }
                if (value === '' || value === '0x' || value === '0X') {
                    return '0x';
                }
                if (value.startsWith('0x') || value.startsWith('0X')) {
                    return '0x' + value.slice(2).toLowerCase();
                }
                return '0x' + value.toLowerCase();
            }

            function hexToBytes(value){
                const hex = normalizeHex(value).slice(2);
                if (hex.length === 0) {
                    return new Uint8Array(0);
                }
                const padded = hex.length % 2 === 0 ? hex : '0' + hex;
                const out = new Uint8Array(padded.length / 2);
                for (let i = 0; i < padded.length; i += 2) {
                    out[i >> 1] = parseInt(padded.slice(i, i + 2), 16);
                }
                return out;
            }
    "#;

    ctx.eval(Source::from_bytes(helpers.as_bytes()))
        .map_err(|e| anyhow::anyhow!(format!("install step helpers failed: {e:?}")))?;

    Ok(())
}

fn dispatch_host_call<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    method: HostMethod,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    match method {
        HostMethod::GetBalance => host_get_balance(env, payload),
        HostMethod::GetNonce => host_get_nonce(env, payload),
        HostMethod::GetCode => host_get_code(env, payload),
        HostMethod::GetState => host_get_state(env, payload),
        HostMethod::Exists => host_exists(env, payload),
    }
}

fn host_get_balance<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x0".to_string());
    };

    let balance_value = resolve_balance(env, address)?;

    Ok(format_hex_u256(balance_value))
}

fn host_get_nonce<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x0".to_string());
    };

    let nonce = env
        .state_view
        .borrow_mut()
        .account_nonce(address)
        .ok_or(anyhow::anyhow!("Account {address:?} not found in a state"))?;

    Ok(format!("0x{nonce:x}"))
}

fn host_get_code<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("0x".to_string());
    };

    if let Some(entry) = env.code_overlay.borrow().get(&address) {
        return if let Some(code) = &entry.value {
            Ok(format!("0x{}", alloy::primitives::hex::encode(code)))
        } else {
            Ok("0x".to_string())
        };
    }

    let code = {
        let mut state_view = env.state_view.borrow_mut();
        let props = state_view
            .get_account(address)
            .ok_or(anyhow::anyhow!("Account {address:?} not found in a state"))?;
        get_code(&mut *state_view, &props)
    };

    Ok(format!("0x{}", alloy::primitives::hex::encode(code)))
}

fn host_get_state<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("address is not supplied in getState"))?;
    let slot = payload
        .get("slot")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("slot is not supplied in getState"))?;

    let (Some(address), Some(key)) = (parse_address(addr), parse_b256(slot)) else {
        return Ok("0x0".to_string());
    };

    if let Some(entry) = env.storage_overlay.borrow().get(&(address, key)) {
        return Ok(format!(
            "0x{}",
            alloy::primitives::hex::encode(entry.value.0)
        ));
    }

    let flat = derive_flat_storage_key(&B160::from_be_bytes(address.into_array()), &(key.0.into()));
    let value = env
        .state_view
        .borrow_mut()
        .read(B256::from(flat.as_u8_array()))
        .unwrap_or_default();

    Ok(format!("0x{}", alloy::primitives::hex::encode(value.0)))
}

fn host_exists<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    payload: &JsonValue,
) -> anyhow::Result<String> {
    let addr = payload
        .get("address")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let Some(address) = parse_address(addr) else {
        return Ok("false".to_string());
    };

    if let Some(entry) = env.code_overlay.borrow().get(&address) {
        return Ok(entry.value.is_some().to_string());
    }

    if resolve_balance(env, address)? > U256::ZERO {
        return Ok("true".to_string());
    }

    if env
        .storage_overlay
        .borrow()
        .keys()
        .any(|(addr_ref, _)| *addr_ref == address)
    {
        return Ok("true".to_string());
    }

    let exists = env.state_view.borrow_mut().get_account(address).is_some();

    Ok(exists.to_string())
}

fn resolve_balance<V: ViewState + 'static>(
    env: &HostEnvironment<V>,
    address: Address,
) -> anyhow::Result<U256> {
    let delta = {
        let balance_overlay = env.balance_overlay.borrow();
        balance_overlay
            .get(&address)
            .map(|entry| entry.value.clone())
    };

    let mut balance = env
        .state_view
        .borrow_mut()
        .get_account(address)
        .map(|props| props.balance)
        .unwrap_or_default();

    if let Some(delta) = delta {
        let (with_add, overflow) = balance.overflowing_add(delta.added);
        if overflow {
            anyhow::bail!("balance overflow when applying balance delta for account {address:?}");
        }

        balance = with_add;
        if delta.removed != U256::ZERO {
            let (with_sub, overflow) = balance.overflowing_sub(delta.removed);
            if overflow {
                anyhow::bail!(
                    "balance underflow when applying balance delta for account {address:?}"
                );
            }
            balance = with_sub;
        }
    }

    Ok(balance)
}
