//! Derive macro for the ee plugin SDK.

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ImplItem, ItemImpl, LitStr, Result, Token, parse_macro_input};

struct PluginArgs {
    name: Option<LitStr>,
}

impl Parse for PluginArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        while !input.is_empty() {
            let key: syn::Ident = input.parse()?;
            if key != "name" {
                return Err(syn::Error::new(key.span(), "unsupported #[plugin] argument"));
            }
            input.parse::<Token![=]>()?;
            if name.is_some() {
                return Err(syn::Error::new(key.span(), "duplicate `name` argument"));
            }
            name = Some(input.parse()?);
            if input.is_empty() {
                break;
            }
            input.parse::<Token![,]>()?;
        }
        Ok(Self { name })
    }
}

/// Implement `ee_plugin_api::Plugin` for the type in an inherent `impl` block.
///
/// The block must define:
///
/// ```ignore
/// fn apply(&mut self, ctx: &ee_plugin_api::PluginContext) -> Result<(), ee_plugin_api::PluginError>
/// ```
///
/// The optional `name = "..."` argument sets `Plugin::name`. Without it, the
/// type identifier is used.
///
/// # Examples
///
/// ```ignore
/// struct Demo;
///
/// #[plugin(name = "demo")]
/// impl Demo {
///     fn apply(&mut self, ctx: &PluginContext) -> Result<(), PluginError> {
///         Ok(())
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn plugin(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as PluginArgs);
    let input = parse_macro_input!(item as ItemImpl);

    let has_apply = input
        .items
        .iter()
        .any(|item| matches!(item, ImplItem::Fn(function) if function.sig.ident == "apply"));

    if !has_apply {
        return syn::Error::new_spanned(
            &input.self_ty,
            "#[plugin] impl block must define an `apply` method",
        )
        .to_compile_error()
        .into();
    }

    let self_ty = &input.self_ty;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let name = match args.name {
        Some(name) => quote! { #name },
        None => {
            let type_name = match self_ty.as_ref() {
                syn::Type::Path(path) => path
                    .path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_else(|| "plugin".to_owned()),
                _ => "plugin".to_owned(),
            };
            quote! { #type_name }
        }
    };

    let plugin_impl = quote! {
        impl #impl_generics ::ee_plugin_api::Plugin for #self_ty #where_clause {
            fn name(&self) -> &str {
                #name
            }

            fn apply(
                &mut self,
                ctx: &::ee_plugin_api::PluginContext,
            ) -> ::core::result::Result<(), ::ee_plugin_api::PluginError> {
                <Self>::apply(self, ctx)
            }
        }
    };

    // `ty_generics` is retained for clarity even though the trait impl uses the
    // full self type; this keeps generic impls well-formed.
    let _ = ty_generics;

    TokenStream::from(quote! {
        #input
        #plugin_impl
    })
}
