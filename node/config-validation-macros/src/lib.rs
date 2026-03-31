//! Declarative validation derive for the node's end configuration.
//!
//! `ConfigValidate` generates an implementation of `crate::config::ConfigValidate`
//! for the annotated struct. When the struct is also marked with
//! `#[config_validate(root)]`, the derive additionally wires root-level async
//! validations into `ConfigValidate::validate()`,
//! which:
//!
//! - walks the config tree and collects synchronous validation errors
//! - runs async field validators and lets them append more validation errors
//! - formats all collected errors into a single `anyhow::Error`
//!
//! Supported `#[config_validate(...)]` forms on structs:
//!
//! - `root`
//!   Marks the final top-level config struct and enables `validate()`.
//!
//! Supported `#[config_validate(...)]` forms on fields:
//!
//! - `required_if = <role>`
//!   Requires an `Option<_>` field to be `Some` when
//!   `root.general_config.node_role == <role>`.
//! - `custom(<predicate>, <message>)`
//!   Adds a custom synchronous validator. The predicate receives `(&Config, &FieldType)`
//!   and must return `bool`. The message is appended after the generated config path.
//! - `async_validate(<validator>)`
//!   Adds a custom async validator for a field on the root struct. The validator receives
//!   `(&RootConfig, &FieldType, &mut Vec<ValidationError>)` and returns
//!   `anyhow::Result<()>`.
//! - `nested`
//!   Forces recursive validation for this field even if it does not match the default
//!   recursion heuristics.
//! - `skip_nested`
//!   Disables recursive validation for this field even if it would recurse by default.
//! - `path = "..."`
//!   Overrides the config path segment used in error messages for this field.
//!
//! Default path segment:
//!
//! - if the field name ends with `_config`, that suffix is stripped
//! - otherwise the full field name is used
//!
//! Default recursive validation:
//!
//! - fields whose name ends with `_config`
//! - fields marked with `#[config(nest)]`
//!
//! This keeps common subconfig fields zero-configuration while still allowing explicit
//! opt-out via `skip_nested`.
//!
use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Data, DeriveInput, Expr, Field, Fields, Ident, LitStr, Result, Token, Type,
    TypePath, parse_macro_input, spanned::Spanned,
};

#[derive(Default)]
struct StructAttrs {
    root: bool,
}

enum ValidatorKind {
    RequiredIf(Box<Expr>),
    Custom {
        predicate: Box<Expr>,
        message: Box<Expr>,
    },
}

#[derive(Default)]
struct FieldAttrs {
    path: Option<LitStr>,
    skip_nested: bool,
    nested: bool,
    validators: Vec<ValidatorKind>,
    async_validators: Vec<Expr>,
}

struct ValidationArgs {
    predicate: Expr,
    message: Expr,
}

impl Parse for ValidationArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let predicate = input.parse()?;
        input.parse::<Token![,]>()?;
        let message = input.parse()?;
        Ok(Self { predicate, message })
    }
}

fn parse_struct_attrs(attrs: &[Attribute]) -> Result<StructAttrs> {
    let mut parsed = StructAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("config_validate") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("root") {
                parsed.root = true;
                Ok(())
            } else {
                Err(meta.error("unsupported `config_validate` attribute"))
            }
        })?;
    }
    Ok(parsed)
}

fn parse_field_attrs(field: &Field) -> Result<FieldAttrs> {
    let mut parsed = FieldAttrs::default();
    for attr in &field.attrs {
        if !attr.path().is_ident("config_validate") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("path") {
                parsed.path = Some(meta.value()?.parse()?);
                Ok(())
            } else if meta.path.is_ident("skip_nested") {
                parsed.skip_nested = true;
                Ok(())
            } else if meta.path.is_ident("nested") {
                parsed.nested = true;
                Ok(())
            } else if meta.path.is_ident("required_if") {
                ensure_option_field(field, "required_if")?;
                parsed.validators.push(ValidatorKind::RequiredIf(Box::new(
                    meta.value()?.parse::<Expr>()?,
                )));
                Ok(())
            } else if meta.path.is_ident("custom") {
                let args = meta.input.parse::<ParenValidationArgs>()?;
                parsed.validators.push(ValidatorKind::Custom {
                    predicate: Box::new(args.predicate),
                    message: Box::new(args.message),
                });
                Ok(())
            } else if meta.path.is_ident("async_validate") {
                let args = meta.input.parse::<ParenExpr>()?;
                parsed.async_validators.push(args.expr);
                Ok(())
            } else {
                Err(meta.error("unsupported `config_validate` field attribute"))
            }
        })?;
    }
    if parsed.nested && parsed.skip_nested {
        return Err(syn::Error::new(
            field.span(),
            "`nested` and `skip_nested` cannot be used together",
        ));
    }
    Ok(parsed)
}

fn ensure_option_field(field: &Field, attr_name: &str) -> Result<()> {
    if option_inner_type(&field.ty).is_none() {
        Err(syn::Error::new(
            field.ty.span(),
            format!("`{attr_name}` can only be used on Option fields"),
        ))
    } else {
        Ok(())
    }
}

fn option_inner_type(ty: &Type) -> Option<&Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let segment = path.segments.last()?;
    if segment.ident != "Option" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    if args.args.len() != 1 {
        return None;
    }
    match &args.args[0] {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

struct ParenExpr {
    expr: Expr,
}

impl Parse for ParenExpr {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let content;
        syn::parenthesized!(content in input);
        Ok(Self {
            expr: content.parse()?,
        })
    }
}

struct ParenValidationArgs {
    predicate: Expr,
    message: Expr,
}

impl Parse for ParenValidationArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let content;
        syn::parenthesized!(content in input);
        let args = content.parse::<ValidationArgs>()?;
        Ok(Self {
            predicate: args.predicate,
            message: args.message,
        })
    }
}

/// Derives declarative config validation.
///
/// Example:
///
/// ```rust,ignore
/// #[derive(ConfigValidate)]
/// #[config_validate(root)]
/// pub struct Config {
///     pub general_config: GeneralConfig,
///     #[config_validate(required_if = NodeRole::MainNode)]
///     pub external_price_api_client_config: Option<ExternalPriceApiClientConfig>,
/// }
///
/// #[derive(ConfigValidate)]
/// pub struct GeneralConfig {
///     #[config_validate(required_if = NodeRole::ExternalNode)]
///     pub main_node_rpc_url: Option<String>,
/// }
/// ```
#[proc_macro_derive(ConfigValidate, attributes(config_validate))]
pub fn derive_config_validate(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.into_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> Result<proc_macro2::TokenStream> {
    let name = input.ident;
    let struct_attrs = parse_struct_attrs(&input.attrs)?;
    let Data::Struct(data) = input.data else {
        return Err(syn::Error::new(
            name.span(),
            "`ConfigValidate` only supports structs",
        ));
    };
    let Fields::Named(fields) = data.fields else {
        return Err(syn::Error::new(
            name.span(),
            "`ConfigValidate` requires named fields",
        ));
    };

    let mut field_validations = Vec::new();
    let mut nested_validations = Vec::new();
    let mut async_field_validations = Vec::new();

    for field in &fields.named {
        let field_ident = field.ident.as_ref().unwrap();
        let field_attrs = parse_field_attrs(field)?;
        let should_recurse = should_recurse(field, field_ident, &field_attrs);
        let default_path = default_path_segment(field_ident);
        let path_segment = field_attrs.path.unwrap_or(default_path);

        for validator in field_attrs.validators {
            let validation = match validator {
                ValidatorKind::RequiredIf(required_role) => quote! {
                    let required_role = #required_role;
                    if root.general_config.node_role == required_role && self.#field_ident.is_none() {
                        let path = crate::config::join_validation_path(prefix, #path_segment);
                        errors.push(crate::config::ValidationError::new(
                            path,
                            format!("is required when `general.node_role={}`", required_role),
                        ));
                    }
                },
                ValidatorKind::Custom { predicate, message } => quote! {
                    if !((#predicate)(root, &self.#field_ident)) {
                        let path = crate::config::join_validation_path(prefix, #path_segment);
                        errors.push(crate::config::ValidationError::new(path, #message));
                    }
                },
            };
            field_validations.push(validation);
        }

        if should_recurse {
            nested_validations.push(nested_validation(field, field_ident, &path_segment));
        }

        for validator in field_attrs.async_validators {
            async_field_validations.push(quote! {
                (#validator)(self, &self.#field_ident, errors).await?;
            });
        }
    }

    if !struct_attrs.root && !async_field_validations.is_empty() {
        return Err(syn::Error::new(
            name.span(),
            "`async_validate` can only be used on the root config struct",
        ));
    }

    let validate_async_method = if struct_attrs.root {
        quote! {
            async fn validate_async(
                &self,
                errors: &mut ::std::vec::Vec<crate::config::ValidationError>,
            ) -> ::anyhow::Result<()> {
                #(#async_field_validations)*
                Ok(())
            }
        }
    } else {
        proc_macro2::TokenStream::new()
    };

    Ok(quote! {
        #[async_trait::async_trait(?Send)]
        impl crate::config::ConfigValidate for #name {
            fn validate_conditional(
                &self,
                root: &crate::config::Config,
                errors: &mut ::std::vec::Vec<crate::config::ValidationError>,
                prefix: &str,
            ) {
                #(#field_validations)*
                #(#nested_validations)*
            }

            #validate_async_method
        }
    })
}

fn default_path_segment(field_ident: &Ident) -> LitStr {
    let field_name = field_ident.to_string();
    let path = field_name
        .strip_suffix("_config")
        .unwrap_or(&field_name)
        .to_owned();
    LitStr::new(&path, field_ident.span())
}

fn should_recurse(field: &Field, field_ident: &Ident, field_attrs: &FieldAttrs) -> bool {
    if field_attrs.skip_nested {
        return false;
    }
    if field_attrs.nested || field_ident.to_string().ends_with("_config") {
        return true;
    }
    has_smart_config_nest(field)
}

fn has_smart_config_nest(field: &Field) -> bool {
    for attr in &field.attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        let mut has_nest = false;
        let parse_result = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("nest") {
                has_nest = true;
            }
            Ok(())
        });
        if parse_result.is_ok() && has_nest {
            return true;
        }
    }
    false
}

fn nested_validation(
    field: &Field,
    field_ident: &Ident,
    path_segment: &LitStr,
) -> proc_macro2::TokenStream {
    if let Some(inner_ty) = option_inner_type(&field.ty) {
        quote! {
            if let Some(value) = &self.#field_ident {
                use crate::config::MaybeConditionalConfigValidator as _;

                let child_prefix = crate::config::join_validation_path(prefix, #path_segment);
                let receiver = &::std::marker::PhantomData::<#inner_ty>;
                receiver.maybe_validate_conditional(value, root, errors, &child_prefix);
            }
        }
    } else {
        let field_ty = &field.ty;
        quote! {
            use crate::config::MaybeConditionalConfigValidator as _;

            let child_prefix = crate::config::join_validation_path(prefix, #path_segment);
            let receiver = &::std::marker::PhantomData::<#field_ty>;
            receiver.maybe_validate_conditional(&self.#field_ident, root, errors, &child_prefix);
        }
    }
}
