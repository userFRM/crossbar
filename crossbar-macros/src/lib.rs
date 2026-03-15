// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Procedural macros for the crossbar URI router.
//!
//! Provides:
//! - `#[handler]` — transform typed extractor functions into crossbar handlers

use proc_macro::TokenStream;
use quote::quote;
use syn::parse_macro_input;

// ── #[handler] attribute macro ─────────────────────────

/// Transforms a function with typed extractors into a crossbar handler.
///
/// # Extractor Attributes
///
/// - `#[path("name")]` — extracts a path parameter by name. Returns 400 if
///   missing (unless the type is `Option<String>`).
/// - `#[query("name")]` — extracts a query parameter by name. Returns 400 if
///   missing (unless the type is `Option<String>`).
/// - No attribute — passes the raw `Request` through.
///
/// # Example
///
/// ```rust,ignore
/// use crossbar_inproc::prelude::*;
/// use crossbar_macros::handler;
///
/// #[handler]
/// fn get_user(
///     #[path("id")] id: String,
///     #[query("verbose")] verbose: Option<String>,
/// ) -> String {
///     format!("user {} (verbose: {})", id, verbose.is_some())
/// }
/// ```
#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as syn::ItemFn);
    match expand_handler(input_fn) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_handler(mut input_fn: syn::ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    let fn_name = &input_fn.sig.ident;
    let vis = &input_fn.vis;

    if input_fn.sig.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            fn_name,
            "#[handler] does not support async functions — crossbar handlers are synchronous",
        ));
    }

    // Parse the original return type.
    let original_ret_ty = match &input_fn.sig.output {
        syn::ReturnType::Default => quote! { () },
        syn::ReturnType::Type(_, ty) => quote! { #ty },
    };

    // Collect extraction code for each parameter.
    let mut extraction_stmts = Vec::new();
    let params = std::mem::take(&mut input_fn.sig.inputs);

    for param in &params {
        match param {
            syn::FnArg::Typed(pat_type) => {
                let pat = &pat_type.pat;
                let ty = &pat_type.ty;
                let attrs = &pat_type.attrs;

                if let Some(stmt) = build_extraction(pat, ty, attrs)? {
                    extraction_stmts.push(stmt);
                } else {
                    // No extractor attribute — inject the Request directly.
                    extraction_stmts.push(quote! {
                        let #pat: #ty = __crossbar_req;
                    });
                }
            }
            syn::FnArg::Receiver(r) => {
                return Err(syn::Error::new_spanned(
                    r,
                    "#[handler] does not support self parameters",
                ));
            }
        }
    }

    // Collect the original function body.
    let body = &input_fn.block;

    let expanded = quote! {
        #vis fn #fn_name(
            __crossbar_req: crossbar_inproc::types::Request,
        ) -> ::core::result::Result<#original_ret_ty, crossbar_inproc::types::Response> {
            #(#extraction_stmts)*
            ::core::result::Result::Ok(#body)
        }
    };

    Ok(expanded)
}

/// Determines if a type is `Option<...>`.
fn is_option_type(ty: &syn::Type) -> bool {
    if let syn::Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            return segment.ident == "Option";
        }
    }
    false
}

/// Builds the extraction statement for a single parameter based on its
/// attributes (`#[path("name")]`, `#[query("name")]`).
///
/// Returns `None` if no extractor attribute is found (passthrough as Request).
fn build_extraction(
    pat: &syn::Pat,
    ty: &syn::Type,
    attrs: &[syn::Attribute],
) -> syn::Result<Option<proc_macro2::TokenStream>> {
    for attr in attrs {
        if attr.path().is_ident("path") {
            let param_name: syn::LitStr = attr.parse_args()?;
            let name_val = param_name.value();

            if is_option_type(ty) {
                return Ok(Some(quote! {
                    let #pat: #ty = __crossbar_req.path_param(#name_val).map(|s| s.to_string());
                }));
            } else {
                let msg = format!("missing path param: {}", name_val);
                return Ok(Some(quote! {
                    let #pat: #ty = __crossbar_req
                        .path_param(#name_val)
                        .ok_or_else(|| crossbar_inproc::types::Response::bad_request(#msg))?
                        .to_string();
                }));
            }
        } else if attr.path().is_ident("query") {
            let param_name: syn::LitStr = attr.parse_args()?;
            let name_val = param_name.value();

            if is_option_type(ty) {
                return Ok(Some(quote! {
                    let #pat: #ty = __crossbar_req.query_param(#name_val);
                }));
            } else {
                let msg = format!("missing query param: {}", name_val);
                return Ok(Some(quote! {
                    let #pat: #ty = __crossbar_req
                        .query_param(#name_val)
                        .ok_or_else(|| crossbar_inproc::types::Response::bad_request(#msg))?;
                }));
            }
        }
    }
    Ok(None)
}
