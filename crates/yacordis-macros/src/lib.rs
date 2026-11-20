//! Attribute macros for declaring cordis components from plain functions.
//!
//! ```
//! # use yacordis_macros::component;
//! # use yacordis::Schema;
//! #[component(
//!     name = "database",
//!     inject(required = ["config"], optional = ["logger"]),
//!     provides = ["db"],
//!     schema = Schema::object().req("uri", Schema::string()).build()
//! )]
//! fn database(ctx: &yacordis::Context, config: &serde_json::Value) -> yacordis::ComponentResult {
//!     ctx.provide("db", std::sync::Arc::new(config.clone()))?;
//!     Ok(())
//! }
//! // `Database` is a struct implementing `yacordis::Component`.
//! # let _ = Database;
//! ```
//!
//! The generated adapter delegates `apply` to the annotated function;
//! `inject`/`provides` come from the attribute, so dependency-cycle
//! diagnostics (and reactive gating) see the declarations up front,
//! before the body runs. `schema` is an arbitrary expression evaluating to
//! [`yacordis::Schema`] — install and update then validate and resolve
//! the config before the body runs (see `yacordis::schema`). The
//! component struct inherits the function's visibility and its
//! `Component::name` defaults to the function name.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{ItemFn, LitStr, Token, parse_macro_input};

/// Attribute arguments: `name = "…"`, `required = […]`, `optional = […]`,
/// `provides = […]`, `schema = <expr>`.
struct ComponentArgs {
    name: Option<LitStr>,
    required: Vec<LitStr>,
    optional: Vec<LitStr>,
    provides: Vec<LitStr>,
    schema: Option<syn::Expr>,
}

impl syn::parse::Parse for ComponentArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut args = Self {
            name: None,
            required: Vec::new(),
            optional: Vec::new(),
            provides: Vec::new(),
            schema: None,
        };
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            let key_str = key.to_string();
            match key_str.as_str() {
                "name" => {
                    input.parse::<Token![=]>()?;
                    args.name = Some(input.parse()?);
                }
                "required" => {
                    input.parse::<Token![=]>()?;
                    args.required = parse_string_list(input)?;
                }
                "optional" => {
                    input.parse::<Token![=]>()?;
                    args.optional = parse_string_list(input)?;
                }
                "provides" => {
                    input.parse::<Token![=]>()?;
                    args.provides = parse_string_list(input)?;
                }
                "schema" => {
                    input.parse::<Token![=]>()?;
                    args.schema = Some(input.parse()?);
                }
                "inject" => {
                    // Grouped form: inject(required = […], optional = […]).
                    let content;
                    syn::parenthesized!(content in input);
                    while !content.is_empty() {
                        let nested: syn::Ident = content.parse()?;
                        content.parse::<Token![=]>()?;
                        match nested.to_string().as_str() {
                            "required" => args.required = parse_string_list(&content)?,
                            "optional" => args.optional = parse_string_list(&content)?,
                            other => {
                                return Err(syn::Error::new(
                                    nested.span(),
                                    format!(
                                        "unknown inject key `{other}`; expected required, optional"
                                    ),
                                ));
                            }
                        }
                        if !content.is_empty() {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown #[component] key `{other}`; expected name, inject, required, optional, provides, schema"
                        ),
                    ));
                }
            }
            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(args)
    }
}

fn parse_string_list(input: syn::parse::ParseStream) -> syn::Result<Vec<LitStr>> {
    let content;
    syn::bracketed!(content in input);
    let list = Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?;
    Ok(list.into_iter().collect())
}

fn pascal_case(snake: &str) -> String {
    snake
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Declare a cordis component from a plain function. See the crate docs.
#[proc_macro_attribute]
pub fn component(args: TokenStream, item: TokenStream) -> TokenStream {
    let attr_args = parse_macro_input!(args as ComponentArgs);
    let func = parse_macro_input!(item as ItemFn);

    if func.sig.inputs.len() != 2 {
        return syn::Error::new(
            func.sig.inputs.span(),
            "#[component] functions take exactly (ctx: &yacordis::Context, config: &serde_json::Value)",
        )
        .to_compile_error()
        .into();
    }
    if func.sig.asyncness.is_some() {
        return syn::Error::new(
            func.sig.asyncness.span(),
            "#[component] functions are synchronous; use ctx.run_blocking for work off the scheduling thread",
        )
        .to_compile_error()
        .into();
    }

    let vis = &func.vis;
    let fn_name = &func.sig.ident;
    let struct_name = format_ident!("{}", pascal_case(&fn_name.to_string()));
    let component_name = attr_args
        .name
        .clone()
        .unwrap_or_else(|| LitStr::new(&fn_name.to_string(), fn_name.span()));
    // Evaluate the literals at expansion time: interpolating `LitStr`
    // directly yields a string-literal token, so `.value()` cannot be
    // applied to it inside the generated code.
    let required: Vec<String> = attr_args.required.iter().map(|lit| lit.value()).collect();
    let optional: Vec<String> = attr_args.optional.iter().map(|lit| lit.value()).collect();
    let provides: Vec<String> = attr_args.provides.iter().map(|lit| lit.value()).collect();

    // With `schema = <expr>`, the declared config schema rides along on
    // the adapter: install/update validate and resolve the config before
    // the body ever runs, with zero macro magic at runtime.
    let schema_impl = attr_args.schema.as_ref().map(|expr| {
        quote! {
            fn schema(&self) -> Option<::yacordis::Schema> {
                Some(#expr)
            }
        }
    });

    let expanded = quote! {
        #func

        #[doc = concat!("Component adapter generated by `#[component]` for `", stringify!(#fn_name), "`.")]
        #vis struct #struct_name;

        impl ::yacordis::Component for #struct_name {
            fn name(&self) -> Option<&str> {
                Some(#component_name)
            }

            fn inject(&self) -> ::yacordis::Inject {
                let inject = ::yacordis::Inject::none();
                #( let inject = inject.req(#required); )*
                #( let inject = inject.opt(#optional); )*
                inject
            }

            fn provides(&self) -> Vec<String> {
                vec![ #( #provides.to_string() ),* ]
            }

            #schema_impl

            fn apply(&self, ctx: &::yacordis::Context, config: &::yacordis::serde_json::Value) -> ::yacordis::ComponentResult {
                #fn_name(ctx, config)
            }
        }
    };
    expanded.into()
}
