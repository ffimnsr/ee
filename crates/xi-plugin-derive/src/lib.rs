use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{ItemStruct, Result, parse_macro_input};

struct XiPluginArgs;

impl Parse for XiPluginArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        if input.is_empty() {
            Ok(Self)
        } else {
            Err(input.error("xi_plugin no longer accepts syntax-specific arguments"))
        }
    }
}

#[proc_macro_attribute]
pub fn xi_plugin(args: TokenStream, item: TokenStream) -> TokenStream {
    parse_macro_input!(args as XiPluginArgs);
    let input = parse_macro_input!(item as ItemStruct);
    let ident = &input.ident;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let plugin_impl = quote! {
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
    };

    TokenStream::from(quote! {
        #input
        #plugin_impl
    })
}
