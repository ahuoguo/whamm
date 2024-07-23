extern crate core;

use cli::{Cmd, WhammCli};
use std::collections::HashMap;

use crate::behavior::builder_visitor::*;
use crate::common::error::ErrorGen;
use crate::emitter::rewriting::module_emitter::{MemoryTracker, ModuleEmitter};
use crate::generator::init_generator::InitGenerator;
use crate::generator::instr_generator::InstrGenerator;
use crate::parser::whamm_parser::*;

pub mod behavior;
mod cli;
pub mod common;
pub mod emitter;
pub mod generator;
pub mod parser;
pub mod verifier;

use crate::behavior::tree::BehaviorTree;
use crate::behavior::visualize::visualization_to_file;
use crate::emitter::rewriting::visiting_emitter::VisitingEmitter;
use crate::parser::types::Whamm;
use crate::verifier::types::SymbolTable;
use crate::verifier::verifier::{build_symbol_table, type_check};
use clap::Parser;
use log::{error, info};
use orca::ir::module::Module as WasmModule;
use project_root::get_project_root;
use std::path::PathBuf;
use std::process::exit;

const MAX_ERRORS: i32 = 15;

fn setup_logger() {
    env_logger::init();
}

fn main() {
    if let Err(e) = try_main() {
        eprintln!("error: {}", e);
        for c in e.iter_chain().skip(1) {
            eprintln!("  caused by {}", c);
        }
        eprintln!("{}", e.backtrace());
        exit(1)
    }
}

fn try_main() -> Result<(), failure::Error> {
    setup_logger();

    // Get information from user command line args
    let cli = WhammCli::parse();

    match cli.command {
        Cmd::Info {
            spec,
            globals,
            functions,
        } => {
            run_info(spec, globals, functions);
        }
        Cmd::Instr(args) => {
            run_instr(
                args.app,
                args.script,
                args.output_path,
                args.virgil,
                args.run_verifier,
            );
        }
        Cmd::VisScript {
            script,
            run_verifier,
            output_path,
        } => {
            run_vis_script(script, run_verifier, output_path);
        }
    }

    Ok(())
}

/// create output path if it doesn't exist
fn try_path(path: &String) {
    if !PathBuf::from(path).exists() {
        std::fs::create_dir_all(PathBuf::from(path).parent().unwrap()).unwrap();
    }
}

fn run_info(spec: String, print_globals: bool, print_functions: bool) {
    // Parse the script and generate the information
    let mut err = ErrorGen::new("".to_string(), spec.clone(), MAX_ERRORS);
    print_info(spec, print_globals, print_functions, &mut err);

    err.fatal_report("PrintInfo");
}

fn run_instr(
    app_wasm_path: String,
    script_path: String,
    output_wasm_path: String,
    _emit_virgil: bool,
    run_verifier: bool,
) {
    // Set up error reporting mechanism
    let mut err = ErrorGen::new(script_path.clone(), "".to_string(), MAX_ERRORS);

    // Process the script
    let mut whamm = get_script_ast(&script_path, &mut err);
    let mut symbol_table = get_symbol_table(&mut whamm, run_verifier, &mut err);
    let (behavior_tree, simple_ast) = build_behavior(&whamm, &mut err);

    // If there were any errors encountered, report and exit!
    err.check_has_errors();

    // Read app Wasm into Orca module
    let buff = std::fs::read(app_wasm_path).unwrap();
    let mut app_wasm = WasmModule::parse_only_module(&buff, false).unwrap();

    // TODO Configure the generator based on target (wizard vs bytecode rewriting)

    // Create the memory tracker
    if app_wasm.memories.len() > 1 {
        // TODO -- make this work with multi-memory
        panic!("only single memory is supported")
    };
    let mut mem_tracker = MemoryTracker {
        mem_id: 0,                  // Assuming the ID of the first memory is 0!
        curr_mem_offset: 1_052_576, // Set default memory base address to DEFAULT + 4KB = 1048576 bytes + 4000 bytes = 1052576 bytes
        emitted_strings: HashMap::new(),
    };

    // Phase 0 of instrumentation (emit globals and provided fns)
    let mut init = InitGenerator {
        emitter: ModuleEmitter::new(&mut app_wasm, &mut symbol_table, &mut mem_tracker),
        context_name: "".to_string(),
        err: &mut err,
    };
    init.run(&mut whamm);
    // If there were any errors encountered, report and exit!
    err.check_has_errors();

    // Phase 1 of instrumentation (actually emits the instrumentation code)
    // This structure is necessary since we need to have the fns/globals injected (a single time)
    // and ready to use in every body/predicate.
    let mut instr = InstrGenerator::new(
        &behavior_tree,
        VisitingEmitter::new(&mut app_wasm, &mut symbol_table, &mem_tracker),
        simple_ast,
        &mut err,
    );
    instr.run(&behavior_tree);
    // If there were any errors encountered, report and exit!
    err.check_has_errors();

    try_path(&output_wasm_path);
    if let Err(e) = app_wasm.emit_wasm(&output_wasm_path) {
        err.add_error(ErrorGen::get_unexpected_error(
            true,
            Some(format!(
                "Failed to dump instrumented wasm to {} from error: {}",
                &output_wasm_path, e
            )),
            None,
        ))
    }
    // If there were any errors encountered, report and exit!
    err.check_has_errors();
}

fn run_vis_script(script_path: String, run_verifier: bool, output_path: String) {
    // Set up error reporting mechanism
    let mut err = ErrorGen::new(script_path.clone(), "".to_string(), MAX_ERRORS);

    let mut whamm = get_script_ast(&script_path, &mut err);
    // building the symbol table is necessary since it does some minor manipulations of the AST
    // (adds declared globals to the script AST node)
    let _symbol_table = get_symbol_table(&mut whamm, run_verifier, &mut err);
    let (behavior_tree, ..) = build_behavior(&whamm, &mut err);

    // if there are any errors, should report and exit!
    err.check_has_errors();

    if !PathBuf::from(&output_path).exists() {
        std::fs::create_dir_all(PathBuf::from(&output_path).parent().unwrap()).unwrap();
    }

    let path = match get_pb(&PathBuf::from(output_path.clone())) {
        Ok(pb) => pb,
        Err(_) => exit(1),
    };

    if visualization_to_file(&behavior_tree, path).is_ok() {
        if let Err(err) = opener::open(output_path.clone()) {
            error!("Could not open visualization tree at: {}", output_path);
            error!("{:?}", err)
        }
    }
    exit(0);
}

fn get_symbol_table(ast: &mut Whamm, run_verifier: bool, err: &mut ErrorGen) -> SymbolTable {
    let mut st = build_symbol_table(ast, err);
    err.check_too_many();
    verify_ast(ast, &mut st, run_verifier, err);
    st
}

fn verify_ast(ast: &Whamm, st: &mut SymbolTable, run_verifier: bool, err: &mut ErrorGen) {
    if run_verifier && !type_check(ast, st, err) {
        error!("AST failed verification!");
    }
    err.check_too_many();
}

fn get_script_ast(script_path: &String, err: &mut ErrorGen) -> Whamm {
    match std::fs::read_to_string(script_path) {
        Ok(unparsed_str) => {
            // Parse the script and build the AST
            match parse_script(&unparsed_str, err) {
                Some(ast) => {
                    info!("successfully parsed");
                    err.check_too_many();
                    ast
                }
                None => {
                    err.report();
                    exit(1);
                }
            }
        }
        Err(error) => {
            error!("Cannot read specified file {}: {}", script_path, error);
            exit(1);
        }
    }
}

fn build_behavior(whamm: &Whamm, err: &mut ErrorGen) -> (BehaviorTree, SimpleAST) {
    // Build the behavior tree from the AST
    let mut simple_ast = SimpleAST::new();
    let mut behavior = build_behavior_tree(whamm, &mut simple_ast, err);
    err.check_too_many();
    behavior.reset();

    (behavior, simple_ast)
}

pub(crate) fn get_pb(file_pb: &PathBuf) -> Result<PathBuf, String> {
    if file_pb.is_relative() {
        match get_project_root() {
            Ok(r) => {
                let mut full_path = r.clone();
                full_path.push(file_pb);
                Ok(full_path)
            }
            Err(e) => Err(format!("the root folder does not exist: {:?}", e)),
        }
    } else {
        Ok(file_pb.clone())
    }
}
