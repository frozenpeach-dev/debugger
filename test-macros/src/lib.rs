#![feature(file_create_new)]
extern crate proc_macro2;
use proc_macro;
use quote::{quote, ToTokens};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use syn::{parse_macro_input, Ident, ItemFn, ItemMod, ItemUse};

/// `custom_test` proc macro is used as inner attribute of a test module :
/// ```
/// use crate::test_macros::test_module;
/// mod test {
///     #![test_module]
/// }
/// ```
///
/// It will create a runner that will call every function defined inside the module.
/// The module must end with the declaration of a gateway preceded by the proc macro
/// test_gateway.
#[proc_macro_attribute]
pub fn test_module(
    args: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let path = args.to_string();
    let path = path.strip_suffix("\"").unwrap();
    let path = path.strip_prefix("\"").unwrap();
    let mut file = File::create(path).unwrap();
    let module = parse_macro_input!(item as ItemMod);
    let mut tests = Vec::new();
    let mut tests_sym = Vec::new();
    let mut uses = Vec::new();
    for f in module.content.unwrap().1.iter() {
        if let Ok(ItemFn {
            attrs,
            vis,
            sig,
            block,
        }) = syn::parse::<ItemFn>(proc_macro::TokenStream::from(f.to_token_stream()))
        {
            let stmts = block.stmts;
            let mut name = sig.ident.to_string();
            name.insert_str(0, ".test_");
            let test = quote! {
                #[no_mangle]
                #[inline(never)]
                #[link_section = #name ]
                #(#attrs)* #vis #sig {
                    #(#stmts)*
                }
            };
            tests.push(test);
            tests_sym.push((name.clone(), sig.clone().ident));
            if name != ".test_gateway".to_string() {
                file.write_all(sig.ident.to_string().as_bytes())
                    .unwrap_or_else(|_| panic!("Could not register {}", name));
                file.write_all(b"\n").unwrap();
            }
        } else if let Ok(module) =
            syn::parse::<ItemUse>(proc_macro::TokenStream::from(f.to_token_stream()))
        {
            uses.push(quote!(#module))
        };
    }
    let idents = tests_sym
        .clone()
        .iter()
        .map(|x| x.clone().1)
        .collect::<Vec<Ident>>();
    let runner = quote! (
        pub fn runner() {
            #(#idents();)*
        }
    );

    let a = quote! (
        mod test {
            #(#uses)*
            #(#tests)*
            #runner
        }

    );
    a.into()
}

#[proc_macro_attribute]
pub fn test_gateway(
    _args: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let ItemFn {
        attrs: _attrs,
        vis,
        sig,
        block,
    } = parse_macro_input!(item as ItemFn);
    if sig.ident.to_string() != "gateway".to_string() {
        panic!("Gateway must be named \"gateway\"");
    }
    let stmts = block.stmts;
    let stream = quote! {
        #[no_mangle]
        #[inline(never)]
        #[link_section = ".test_gateway" ]
        #vis #sig {
            #(#stmts)*
        }
    };
    stream.into()
}

#[proc_macro_attribute]
pub fn main(
    args: proc_macro::TokenStream,
    _item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    let mut stmts = vec![];

    // Imports
    stmts.push(quote!(
        use external_tests::instances;
    ));

    // Symbol file
    let symbol_file = get_attr(args.clone(), "symfile").unwrap();

    // Bootable file
    let bootable_file = get_attr(args.clone(), "bootfile").unwrap();

    // Compute GDB Path
    if let Some(path) = get_attr(args.clone(), "gdb") {
        stmts.push(quote!(
                let mut runner = instances::Debugger::new(#path, vec![
            #symbol_file.to_string()
        ], #bootable_file);
            ))
    } else {
        stmts.push(quote!(
                let mut runner = instances::Debugger::new("gdb", vec![
            #symbol_file.to_string()
        ], #bootable_file);
            ))
    }

    // Set arch
    if let Some(arch) = get_attr(args.clone(), "arch") {
        let mut stmt = "set architecture ".to_string();
        stmt.push_str(&arch);
        stmts.push(quote!(
            runner.before(#stmt);
        ))
    }

    // Set port
    if let Some(port) = get_attr(args.clone(), "port") {
        let mut stmt = "target remote ".to_string();
        stmt.push_str(&port);
        stmts.push(quote!(
            runner.before(#stmt);
        ))
    } else {
        stmts.push(quote!(
            runner.before("target remote localhost:1234");
        ))
    }

    // Set default gateway and panic handler
    stmts.push(quote!(
        runner.register_gateway("gateway");
        runner.register_panic("main::panic");
    ));

    let path = get_attr(args.clone(), "path").expect("\"path\" argument is required");
    let file = File::open(path).unwrap();
    let buf = BufReader::new(file);
    let bps: Vec<String> = buf
        .lines()
        .map(|l| l.expect("Could not parse register"))
        .collect();
    let mut bp = Vec::new();
    for b in bps {
        bp.push(quote!(
            runner.register_test(#b);
        ))
    }
    let stream = quote!(
        use external_tests::tokio;
        #[external_tests::tokio::test]
        async fn gdb_runner() {
            env_logger::init();
        #(#stmts)*
        #(#bp)*
        let recap = runner.test().await.unwrap().unwrap();
        dbg!(recap);
        }
    );
    stream.into()
}

fn get_attr(args: proc_macro::TokenStream, key: &str) -> Option<String> {
    let args = args.to_string();
    let mut value = None;
    let mut args: Vec<&str> = args.split(',').collect();
    args.retain(|arg| {
        let map: Vec<&str> = arg.split('=').collect();
        let found = map.first().unwrap();
        let found = found.trim();
        if found == key {
            value = Some(
                map.last()
                    .unwrap()
                    .to_string()
                    .trim()
                    .strip_prefix('\"')
                    .unwrap()
                    .strip_suffix('\"')
                    .unwrap()
                    .to_string(),
            );
        };
        true
    });
    value
}
