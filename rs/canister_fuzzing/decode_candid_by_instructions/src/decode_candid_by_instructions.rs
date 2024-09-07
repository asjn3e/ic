use ic_state_machine_tests::{StateMachine, StateMachineBuilder};
use ic_types::{ingress::WasmResult, CanisterId, Cycles};
use once_cell::sync::Lazy;
use std::cell::RefCell;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::ptr::addr_of;
use std::time::Duration;

use libafl::{
    corpus::inmemory_ondisk::InMemoryOnDiskCorpus,
    events::SimpleEventManager,
    executors::{inprocess::InProcessExecutor, ExitKind},
    feedback_or,
    feedbacks::map::AflMapFeedback,
    feedbacks::CrashFeedback,
    fuzzer::{Fuzzer, StdFuzzer},
    inputs::BytesInput,
    mutators::scheduled::{havoc_mutations, StdScheduledMutator},
    observers::map::hitcount_map::HitcountsMapObserver,
    observers::map::StdMapObserver,
    observers::value::RefCellValueObserver,
    prelude::*,
    schedulers::QueueScheduler,
    stages::mutational::StdMutationalStage,
    state::StdState,
};

// use libafl::monitors::SimpleMonitor;
use libafl::monitors::tui::{ui::TuiUI, TuiMonitor};
use libafl_bolts::{current_nanos, rands::StdRand, tuples::tuple_list, HasLen};
use slog::Level;
mod decode_map;
use decode_map::{DecodingMapFeedback, DECODING_MAP_OBSERVER_NAME, MAP};

// TODO: This should be obtained from env
const EXECUTION_DIR: &str = "/ic/rs/canister_fuzzing/decode_candid_by_instructions";
static mut TEST: Lazy<RefCell<(StateMachine, CanisterId)>> =
    Lazy::new(|| RefCell::new(create_execution_test()));
static mut COVERAGE_MAP: &mut [u8] = &mut [0; 65536];

// TODO: The right way to do this would be iclude_bytes! but would require a build.rs
// since the env var is not set at compile time.
fn read_canister_bytes() -> Vec<u8> {
    let wasm_path = std::path::PathBuf::from(std::env::var("FUZZ_CANISTER_WASM_PATH").unwrap());
    let mut f = File::open(wasm_path).unwrap();
    let mut buffer = Vec::new();
    f.read_to_end(&mut buffer).unwrap();
    buffer
}

fn create_execution_test() -> (StateMachine, CanisterId) {
    let test = StateMachineBuilder::new()
        .no_dts()
        .with_log_level(Some(Level::Critical))
        .build();

    let canister_id = test
        .install_canister_with_cycles(
            read_canister_bytes(),
            vec![],
            None,
            Cycles::new(5_000_000_000_000),
        )
        .unwrap();
    (test, canister_id)
}

pub fn main() {
    let mut harness = |input: &BytesInput| {
        let canister_id = unsafe { TEST.borrow().1 };
        let test = unsafe { &mut TEST.borrow_mut().0 };
        let result = test.execute_ingress(canister_id, "decode", (*input).clone().into());
        let instructions = match result {
            Ok(WasmResult::Reply(result)) => {
                let mut instructions = [0u8; 8];
                instructions.clone_from_slice(&result[0..8]);
                u64::from_le_bytes(instructions)
            }
            Ok(WasmResult::Reject(message)) => {
                // Canister crashing is interesting
                if message.contains("Canister trapped") {
                    return ExitKind::Crash;
                }
                0
            }
            _ => return ExitKind::Ok, // continue
        };

        // TODO: Is this needed?
        test.advance_time(Duration::from_secs(1));
        test.tick();

        let result = test.query(canister_id, "export_coverage", vec![]);
        if let Ok(WasmResult::Reply(result)) = result {
            unsafe { COVERAGE_MAP.copy_from_slice(&result) };
        }

        let ratio = instructions / input.len() as u64;
        let previous_ratio = unsafe { MAP.borrow().previous_ratio };
        let mut decoding_map = unsafe { MAP.borrow_mut() };
        if ratio > previous_ratio {
            decoding_map.increased = true;
            decoding_map.previous_ratio = ratio;
        } else {
            decoding_map.increased = false;
        }

        // The success condition for the fuzzer is cycles consumed to input length ratio is
        // over a certain threshold. Once we reach this condition, the fuzzer creates a crash.
        if ratio > 10_000_000 {
            return ExitKind::Crash;
        }
        ExitKind::Ok
    };

    let decoding_map_observer = unsafe {
        RefCellValueObserver::new(
            DECODING_MAP_OBSERVER_NAME,
            libafl_bolts::ownedref::OwnedRef::from_ptr(addr_of!(MAP)),
        )
    };

    let decoding_feedback = DecodingMapFeedback::new();
    let hitcount_map_observer =
        HitcountsMapObserver::new(unsafe { StdMapObserver::new("coverage_map", COVERAGE_MAP) });
    let afl_map_feedback = AflMapFeedback::new(&hitcount_map_observer);
    let mut feedback = feedback_or!(decoding_feedback, afl_map_feedback);
    let mut objective = CrashFeedback::new();

    let mut state = StdState::new(
        StdRand::with_seed(current_nanos()),
        InMemoryOnDiskCorpus::no_meta(PathBuf::from(format!("{}/input", EXECUTION_DIR))).unwrap(),
        InMemoryOnDiskCorpus::no_meta(PathBuf::from(format!("{}/crashes", EXECUTION_DIR))).unwrap(),
        &mut feedback,
        &mut objective,
    )
    .unwrap();

    // let mon = SimpleMonitor::new(|s| println!("{s}"));

    let ui = TuiUI::with_version(
        String::from("Decode Candid by Instruction / Input Ratio"),
        String::from("0.0.1"),
        false,
    );
    let mon = TuiMonitor::new(ui);

    let mut mgr = SimpleEventManager::new(mon);
    let scheduler = QueueScheduler::new();
    let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

    let mut executor = InProcessExecutor::new(
        &mut harness,
        tuple_list!(decoding_map_observer, hitcount_map_observer),
        &mut fuzzer,
        &mut state,
        &mut mgr,
    )
    .expect("Failed to create the Executor");

    let paths = fs::read_dir(PathBuf::from(format!("{}/corpus", EXECUTION_DIR))).unwrap();
    for path in paths {
        let mut f = File::open(path.unwrap().path()).unwrap();
        let mut buffer = Vec::new();
        f.read_to_end(&mut buffer).unwrap();
        fuzzer
            .evaluate_input(&mut state, &mut executor, &mut mgr, BytesInput::new(buffer))
            .unwrap();
    }

    // [TOOD]
    // We could have an actual Candid random generator
    // let mut generator = RandBytesGenerator::new(32);
    // state
    //     .generate_initial_inputs_forced(&mut fuzzer, &mut executor, &mut generator, &mut mgr, 8)
    //     .expect("Failed to generate the initial corpus");

    let mutator = StdScheduledMutator::new(havoc_mutations());
    let mut stages = tuple_list!(StdMutationalStage::new(mutator));

    fuzzer
        .fuzz_loop(&mut stages, &mut executor, &mut state, &mut mgr)
        .expect("Error in the fuzzing loop");
}
