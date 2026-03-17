use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Attribute, FnArg, ItemFn, PatType, Path, Result, Token, Type, TypePath, parse_macro_input,
};

struct CaseList {
    cases: Punctuated<Path, Token![,]>,
}

impl Parse for CaseList {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let content;
        syn::bracketed!(content in input);
        Ok(Self {
            cases: content.parse_terminated(Path::parse, Token![,])?,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    TestCase,
    Tester,
    TesterBuilder,
}

fn param_kind(arg: &FnArg) -> Result<Option<ParamKind>> {
    let FnArg::Typed(PatType { ty, .. }) = arg else {
        return Err(syn::Error::new_spanned(
            arg,
            "methods with `self` are not supported",
        ));
    };
    Ok(type_kind(ty))
}

fn type_kind(ty: &Type) -> Option<ParamKind> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let ident = path.segments.last()?.ident.to_string();
    match ident.as_str() {
        "TestCase" => Some(ParamKind::TestCase),
        "Tester" => Some(ParamKind::Tester),
        "TesterBuilder" => Some(ParamKind::TesterBuilder),
        _ => None,
    }
}

fn path_matches(path: &syn::Path, expected: &[&str]) -> bool {
    path.segments.len() == expected.len()
        && path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .zip(expected.iter().copied())
            .all(|(actual, expected)| actual == expected)
}

fn split_helper_attrs(
    attrs: Vec<Attribute>,
) -> Result<(Vec<Attribute>, Option<syn::Expr>, Option<TokenStream2>)> {
    let mut output = Vec::with_capacity(attrs.len());
    let mut builder_expr = None;
    let mut runtime_args = None;

    for attr in attrs {
        if attr.path().is_ident("test_builder") {
            if builder_expr.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate `test_builder` attribute",
                ));
            }
            builder_expr = Some(attr.parse_args()?);
        } else if attr.path().is_ident("test_runtime")
            || path_matches(attr.path(), &["tokio", "test"])
        {
            if runtime_args.is_some() {
                return Err(syn::Error::new_spanned(
                    attr,
                    "duplicate runtime test attribute",
                ));
            }
            runtime_args = Some(attr.parse_args::<TokenStream2>()?);
        } else if path_matches(attr.path(), &["test_log", "test"]) {
            continue;
        } else {
            output.push(attr);
        }
    }

    Ok((output, builder_expr, runtime_args))
}

fn case_fn_name(case: &Path) -> Result<syn::Ident> {
    let case_name = case
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(case, "expected a case path"))?
        .ident
        .to_string()
        .to_lowercase();
    Ok(format_ident!("{case_name}"))
}

/// Generates one async test wrapper per `TestCase` path passed to the attribute.
///
/// The macro rewrites the annotated function into a private implementation function and emits a
/// module with one wrapper test per case:
///
/// - each wrapper binds `let case = <path>;`
/// - if the function takes a `TesterBuilder`, the wrapper starts from `case.builder()`
/// - if the function takes a `Tester`, the wrapper builds it with `case.builder().build().await?`
/// - if the function takes a `TestCase`, the wrapper passes the case value directly
/// - each wrapper is annotated with `#[test_log::test(tokio::test)]` by default
///
/// Supported parameter types are:
///
/// - `TestCase`
/// - `TesterBuilder`
/// - `Tester`
///
/// `TestCase` may be combined with either `TesterBuilder` or `Tester`.
/// `TesterBuilder` and `Tester` cannot be used together in the same function signature.
///
/// # Cases
///
/// The attribute expects a bracketed list of case paths:
///
/// ```ignore
/// #[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
/// ```
///
/// Each path should evaluate to a `TestCase`, typically one of the exported constants from
/// `zksync_os_integration_tests`.
///
/// # Builder customization
///
/// Use `#[test_builder(...)]` when every generated case should apply the same builder tweak before
/// the `TesterBuilder` or `Tester` is created. The argument must be a function or closure with the
/// shape `fn(TesterBuilder) -> TesterBuilder`.
///
/// ```ignore
/// #[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
/// #[test_builder(|builder| builder.block_time(Duration::from_secs(5)))]
/// async fn pending_nonce_uses_slow_blocks(tester: Tester) -> anyhow::Result<()> {
///     // `tester` is built from the adjusted builder for each case.
///     Ok(())
/// }
/// ```
///
/// # Runtime customization
///
/// Use `#[test_runtime(...)]` to pass options to the generated `tokio::test(...)` runtime.
///
/// ```ignore
/// #[test_multisetup([CURRENT_TO_L1])]
/// #[test_runtime(flavor = "multi_thread")]
/// async fn does_not_get_stuck(tester: Tester) -> anyhow::Result<()> {
///     Ok(())
/// }
/// ```
///
/// # Examples
///
/// Build and use a ready `Tester`:
///
/// ```ignore
/// use zksync_os_integration_tests::{CURRENT_TO_L1, NEXT_TO_GATEWAY, Tester, test_multisetup};
///
/// #[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
/// async fn basic_rpc_smoke(tester: Tester) -> anyhow::Result<()> {
///     let chain_id = tester.l2_provider.get_chain_id().await?;
///     assert!(chain_id > 0);
///     Ok(())
/// }
/// ```
///
/// Inspect the case without starting the node:
///
/// ```ignore
/// use zksync_os_integration_tests::{CURRENT_TO_L1, NEXT_TO_GATEWAY, TestCase, test_multisetup};
/// use zksync_os_integration_tests::SettlementLayer;
///
/// #[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
/// async fn case_metadata_is_expected(case: TestCase) -> anyhow::Result<()> {
///     match case.settlement_layer {
///         SettlementLayer::L1 | SettlementLayer::Gateway => {}
///     }
///     Ok(())
/// }
/// ```
///
/// Customize the builder inside the test before constructing a `Tester`:
///
/// ```ignore
/// use zksync_os_integration_tests::{CURRENT_TO_L1, TesterBuilder, test_multisetup};
///
/// #[test_multisetup([CURRENT_TO_L1])]
/// async fn prover_flow(builder: TesterBuilder) -> anyhow::Result<()> {
///     let tester = builder.enable_prover().build().await?;
///     tester.prover_tester.wait_for_batch_proven(1).await?;
///     Ok(())
/// }
/// ```
///
/// Use both `TestCase` and `Tester` when assertions need case metadata and a running node:
///
/// ```ignore
/// use zksync_os_integration_tests::{
///     CURRENT_TO_L1, NEXT_TO_GATEWAY, TestCase, Tester, test_multisetup,
/// };
///
/// #[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
/// async fn settlement_layer_matches_runtime(
///     case: TestCase,
///     tester: Tester,
/// ) -> anyhow::Result<()> {
///     let chain_id = tester.l2_provider.get_chain_id().await?;
///     assert!(chain_id > 0, "unexpected case: {:?}", case);
///     Ok(())
/// }
/// ```
///
/// # Compile-time restrictions
///
/// - the annotated function must be `async`
/// - methods taking `self` are not supported
/// - only `TestCase`, `TesterBuilder`, and `Tester` parameters are accepted
/// - `TesterBuilder` and `Tester` cannot be used together in the same function
/// - `#[test_builder(...)]` may be used at most once
/// - `#[test_runtime(...)]` may be used at most once
#[proc_macro_attribute]
pub fn test_multisetup(attr: TokenStream, item: TokenStream) -> TokenStream {
    let cases = parse_macro_input!(attr as CaseList);
    let mut input = parse_macro_input!(item as ItemFn);

    if input.sig.asyncness.is_none() {
        return syn::Error::new_spanned(input.sig.fn_token, "test function must be async")
            .into_compile_error()
            .into();
    }

    let (wrapper_attrs, builder_expr, runtime_args) = match split_helper_attrs(input.attrs) {
        Ok(attrs) => attrs,
        Err(err) => return err.into_compile_error().into(),
    };
    input.attrs = Vec::new();

    let impl_name = format_ident!("{}_impl", input.sig.ident);
    let mod_name = input.sig.ident.clone();
    input.sig.ident = impl_name.clone();

    let mut needs_builder = false;
    let mut needs_tester = false;
    let mut arg_exprs = Vec::with_capacity(input.sig.inputs.len());

    for arg in &input.sig.inputs {
        match param_kind(arg) {
            Ok(Some(ParamKind::TestCase)) => {
                arg_exprs.push(quote!(case));
            }
            Ok(Some(ParamKind::TesterBuilder)) => {
                needs_builder = true;
                arg_exprs.push(quote!(builder.clone()));
            }
            Ok(Some(ParamKind::Tester)) => {
                needs_tester = true;
                arg_exprs.push(quote!(tester));
            }
            Ok(None) => {
                return syn::Error::new_spanned(
                    arg,
                    "supported parameters are `Tester`, `TesterBuilder`, and `TestCase`",
                )
                .into_compile_error()
                .into();
            }
            Err(err) => return err.into_compile_error().into(),
        }
    }

    if needs_builder && needs_tester {
        return syn::Error::new_spanned(
            &input.sig.inputs,
            "`TesterBuilder` and `Tester` cannot be used together in the same test function",
        )
        .into_compile_error()
        .into();
    }

    let builder_setup = if needs_builder || needs_tester {
        if let Some(builder_expr) = builder_expr {
            quote! {
                let builder = {
                    let configure: fn(
                        ::zksync_os_integration_tests::TesterBuilder,
                    ) -> ::zksync_os_integration_tests::TesterBuilder = #builder_expr;
                    let builder: ::zksync_os_integration_tests::TesterBuilder = case.builder();
                    configure(builder)
                };
            }
        } else {
            quote! {
                let builder: ::zksync_os_integration_tests::TesterBuilder = case.builder();
            }
        }
    } else {
        quote! {}
    };
    let tester_setup = if needs_tester {
        quote! {
            let tester = builder.build().await?;
        }
    } else {
        quote! {}
    };
    let test_attr: TokenStream2 = if let Some(runtime_args) = runtime_args {
        if runtime_args.is_empty() {
            quote! {
                #[test_log::test(tokio::test)]
            }
        } else {
            quote! {
                #[test_log::test(tokio::test(#runtime_args))]
            }
        }
    } else {
        quote! {
            #[test_log::test(tokio::test)]
        }
    };

    let wrappers = cases.cases.iter().map(|case| {
        let fn_name = match case_fn_name(case) {
            Ok(name) => name,
            Err(err) => return err.into_compile_error(),
        };
        quote! {
            #test_attr
            #(#wrapper_attrs)*
            async fn #fn_name() -> anyhow::Result<()> {
                let case = #case;
                #builder_setup
                #tester_setup
                #impl_name(#(#arg_exprs),*).await
            }
        }
    });

    quote! {
        mod #mod_name {
            use super::*;

            #input

            #(#wrappers)*
        }
    }
    .into()
}
