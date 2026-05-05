use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{
    Data, DeriveInput, Error, Fields, Ident, ItemStruct, LitStr, Result, Token, Variant,
    parse_macro_input,
};

enum PluginMode {
    Simple,
    Syntax { lang: LitStr },
}

struct XiPluginArgs {
    mode: PluginMode,
}

impl Parse for XiPluginArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        if input.is_empty() {
            return Ok(Self { mode: PluginMode::Simple });
        }

        let mode_ident: Ident = input.parse()?;
        if mode_ident != "syntax" {
            return Err(Error::new(mode_ident.span(), "expected `syntax(lang = \"...\")`"));
        }
        let content;
        syn::parenthesized!(content in input);
        let lang_ident: Ident = content.parse()?;
        if lang_ident != "lang" {
            return Err(Error::new(lang_ident.span(), "expected `lang = \"...\"`"));
        }
        content.parse::<Token![=]>()?;
        let lang: LitStr = content.parse()?;
        if !content.is_empty() {
            return Err(content.error("unexpected trailing tokens in xi_plugin attribute"));
        }
        Ok(Self { mode: PluginMode::Syntax { lang } })
    }
}

#[proc_macro_attribute]
pub fn xi_plugin(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as XiPluginArgs);
    let input = parse_macro_input!(item as ItemStruct);
    let ident = &input.ident;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let plugin_impl = match args.mode {
        PluginMode::Simple => quote! {
            impl #impl_generics ::xi_plugin_lib::Plugin for #ident #ty_generics #where_clause {
                type Cache = ::xi_plugin_lib::ChunkCache;

                fn initialize(&mut self, core: ::xi_plugin_lib::CoreProxy) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::initialize(self, core)
                }

                fn update(
                    &mut self,
                    view: &mut ::xi_plugin_lib::View<Self::Cache>,
                    delta: Option<&::xi_rope::RopeDelta>,
                    edit_type: String,
                    author: String,
                ) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::update(self, view, delta, edit_type, author)
                }

                fn did_save(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, old_path: Option<&std::path::Path>) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::did_save(self, view, old_path)
                }

                fn did_close(&mut self, view: &::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::did_close(self, view)
                }

                fn new_view(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::new_view(self, view)
                }

                fn config_changed(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, changes: &::xi_core_lib::ConfigTable) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::config_changed(self, view, changes)
                }

                fn language_changed(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, old_lang: ::xi_core_lib::LanguageId) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::language_changed(self, view, old_lang)
                }

                fn custom_command(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, method: &str, params: ::serde_json::Value) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::custom_command(self, view, method, params)
                }

                fn idle(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::idle(self, view)
                }

                fn shutdown(&mut self) {
                    <Self as ::xi_plugin_lib::SimplePlugin>::shutdown(self)
                }

                fn get_hover(
                    &mut self,
                    view: &mut ::xi_plugin_lib::View<Self::Cache>,
                    position: usize,
                    cancel: ::tokio_util::sync::CancellationToken,
                ) -> Result<::xi_plugin_lib::Hover, ::xi_rpc::RemoteError> {
                    <Self as ::xi_plugin_lib::SimplePlugin>::get_hover(self, view, position, cancel)
                }
            }
        },
        PluginMode::Syntax { lang } => quote! {
            impl #impl_generics ::xi_plugin_lib::SyntaxDescriptor for #ident #ty_generics #where_clause {
                const LANGUAGE: &'static str = #lang;
            }

            impl #impl_generics ::xi_plugin_lib::Plugin for #ident #ty_generics #where_clause {
                type Cache = ::xi_plugin_lib::StateCache<<Self as ::xi_plugin_lib::SyntaxPlugin>::State>;

                fn initialize(&mut self, core: ::xi_plugin_lib::CoreProxy) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::initialize(self, core)
                }

                fn update(
                    &mut self,
                    view: &mut ::xi_plugin_lib::View<Self::Cache>,
                    delta: Option<&::xi_rope::RopeDelta>,
                    edit_type: String,
                    author: String,
                ) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::update(self, view, delta, edit_type, author)
                }

                fn did_save(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, old_path: Option<&std::path::Path>) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::did_save(self, view, old_path)
                }

                fn did_close(&mut self, view: &::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::did_close(self, view)
                }

                fn new_view(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::new_view(self, view)
                }

                fn config_changed(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, changes: &::xi_core_lib::ConfigTable) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::config_changed(self, view, changes)
                }

                fn language_changed(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, old_lang: ::xi_core_lib::LanguageId) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::language_changed(self, view, old_lang)
                }

                fn custom_command(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>, method: &str, params: ::serde_json::Value) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::custom_command(self, view, method, params)
                }

                fn idle(&mut self, view: &mut ::xi_plugin_lib::View<Self::Cache>) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::idle(self, view)
                }

                fn shutdown(&mut self) {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::shutdown(self)
                }

                fn get_hover(
                    &mut self,
                    view: &mut ::xi_plugin_lib::View<Self::Cache>,
                    position: usize,
                    cancel: ::tokio_util::sync::CancellationToken,
                ) -> Result<::xi_plugin_lib::Hover, ::xi_rpc::RemoteError> {
                    <Self as ::xi_plugin_lib::SyntaxPlugin>::get_hover(self, view, position, cancel)
                }
            }
        },
    };

    TokenStream::from(quote! {
        #input
        #plugin_impl
    })
}

#[proc_macro_derive(SpanType, attributes(span_type))]
pub fn derive_span_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match impl_span_type(&input) {
        Ok(tokens) => TokenStream::from(tokens),
        Err(err) => err.to_compile_error().into(),
    }
}

fn impl_span_type(input: &DeriveInput) -> Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let body = match &input.data {
        Data::Enum(data) => impl_enum_span_type(&data.variants)?,
        Data::Struct(data) => {
            let name = find_span_name(&input.attrs)?.unwrap_or_else(|| default_scope_name(ident));
            let pattern = match &data.fields {
                Fields::Named(_) => quote! { Self { .. } },
                Fields::Unnamed(_) => quote! { Self ( .. ) },
                Fields::Unit => quote! { Self },
            };
            quote! {
                match self {
                    #pattern => #name,
                }
            }
        }
        Data::Union(_) => {
            return Err(Error::new(
                input.ident.span(),
                "SpanType can only be derived for structs and enums",
            ));
        }
    };

    Ok(quote! {
        impl #impl_generics ::xi_plugin_lib::SpanType for #ident #ty_generics #where_clause {
            fn scope_name(&self) -> &'static str {
                #body
            }
        }
    })
}

fn impl_enum_span_type(
    variants: &syn::punctuated::Punctuated<Variant, Token![,]>,
) -> Result<proc_macro2::TokenStream> {
    let mut arms = Vec::new();
    for variant in variants {
        let ident = &variant.ident;
        let name = find_span_name(&variant.attrs)?.unwrap_or_else(|| default_scope_name(ident));
        let pattern = match &variant.fields {
            Fields::Named(_) => quote! { Self::#ident { .. } },
            Fields::Unnamed(_) => quote! { Self::#ident ( .. ) },
            Fields::Unit => quote! { Self::#ident },
        };
        arms.push(quote! { #pattern => #name });
    }
    Ok(quote! {
        match self {
            #( #arms, )*
        }
    })
}

fn find_span_name(attrs: &[syn::Attribute]) -> Result<Option<LitStr>> {
    for attr in attrs {
        if !attr.path().is_ident("span_type") {
            continue;
        }
        let mut result = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                result = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("expected `name = \"...\"`"))
            }
        })?;
        return Ok(result);
    }
    Ok(None)
}

fn default_scope_name(ident: &Ident) -> LitStr {
    let name = ident.to_string();
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index != 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    LitStr::new(&out, ident.span())
}
