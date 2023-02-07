// © 2021, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crate::{VerificationRequest, ViperBackendConfig, jni_utils::JniUtils, ServerMessage};
use log::info;
use prusti_common::{
    config,
    report::log::{report, to_legal_file_name},
    vir::{program_normalization::NormalizationInfo, ToViper},
    Stopwatch,
};
use std::{fs::create_dir_all, path::PathBuf, thread, sync::{mpsc, Arc, self}};
use viper::{
    smt_manager::SmtManager, PersistentCache, Cache, VerificationBackend, VerificationResult, Viper, VerificationContext
};
use viper_sys::wrappers::viper::*;
use std::time;
use futures::{stream::Stream, lock};

pub struct VerificationRequestProcessing {
    mtx_rx_servermsg: lock::Mutex<mpsc::Receiver<ServerMessage>>,
    mtx_tx_verreq: sync::Mutex<mpsc::Sender<VerificationRequest>>,
}

// one structure that lives for all the requests and has a single thread working on all the
// requests sequentially
// on reception of a verification request, we send it through a channel to the already running
// thread
impl VerificationRequestProcessing {
    pub fn new() -> Self {
        let (tx_servermsg, rx_servermsg) = mpsc::channel();
        let (tx_verreq, rx_verreq) = mpsc::channel();
        let mtx_rx_servermsg = lock::Mutex::new(rx_servermsg);
        let mtx_tx_verreq = sync::Mutex::new(tx_verreq);
        let ret = Self {mtx_rx_servermsg: mtx_rx_servermsg, mtx_tx_verreq: mtx_tx_verreq};
        thread::spawn(|| { Self::verification_thread(rx_verreq, tx_servermsg) });
        ret
    }

    fn verification_thread(rx_verreq: mpsc::Receiver<VerificationRequest>, tx_servermsg: mpsc::Sender<ServerMessage>) {
        let mut stopwatch = Stopwatch::start("verification_request_processing", "JVM startup");
        let viper = Arc::new(Viper::new_with_args(&config::viper_home(), config::extra_jvm_args()));
        let mut cache = PersistentCache::load_cache(config::cache_path());
        stopwatch.start_next("attach thread to JVM");
        let verification_context = viper.attach_current_thread();
        stopwatch.finish();
        loop {
            match rx_verreq.recv() {
                Ok(request) => {
                    process_verification_request(&viper, &mut cache, &verification_context, &tx_servermsg, request);
                }
                Err(_) => break,
            }
        }
    }

    pub fn verify<'a>(&'a self, request: VerificationRequest) -> impl Stream<Item = ServerMessage> + 'a {
        self.mtx_tx_verreq
            .lock()
            .unwrap()
            .send(request)
            .unwrap();
        futures::stream::unfold(false, move |done: bool| async move {
            if done {
                return None;
            }
            let msg = self.mtx_rx_servermsg
                .lock()
                .await
                .recv()
                .unwrap();
            let mut done = false;
            if let ServerMessage::Termination(_) = msg {
                done = true;
            }
            Some((msg, done))
        })
    }
}
pub fn process_verification_request(
    viper_arc: &Arc<Viper>,
    cache: impl Cache,
    verification_context: &VerificationContext,
    sender: &mpsc::Sender<ServerMessage>,
    mut request: VerificationRequest,
) {
    let ast_utils = verification_context.new_ast_utils();

    // Only for testing: Check that the normalization is reversible.
    if config::print_hash() {
        debug_assert!({
            let mut program = request.program.clone();
            let normalization_info = NormalizationInfo::normalize_program(&mut program);
            normalization_info.denormalize_program(&mut program);
            program == request.program
        });
    }

    // Normalize the request before reaching the cache.
    let normalization_info = NormalizationInfo::normalize_program(&mut request.program);

    let hash = request.get_hash();
    info!(
        "Verification request hash: {} - for program {}",
        hash,
        request.program.get_name()
    );

    let build_or_dump_viper_program = || {
        let mut stopwatch = Stopwatch::start("prusti-server", "construction of JVM objects");
        let ast_factory = verification_context.new_ast_factory();
        let viper_program = request
            .program
            .to_viper(prusti_common::vir::LoweringContext::default(), &ast_factory);

        if config::dump_viper_program() {
            stopwatch.start_next("dumping viper program");
            dump_viper_program(
                &ast_utils,
                viper_program,
                &request.program.get_name_with_check_mode(),
            );
        }

        viper_program
    };

    // Only for testing: Print the hash and skip verification.
    if config::print_hash() {
        println!(
            "Received verification request for: {}",
            request.program.get_name()
        );
        println!("Hash of the request is: {hash}");
        // Some tests need the dump to report a diff of the Viper programs.
        if config::dump_viper_program() {
            ast_utils.with_local_frame(16, || {
                let _ = build_or_dump_viper_program();
            });
        }
        sender.send(ServerMessage::Termination(viper::VerificationResult::Success)).unwrap();
        return;
    }

    // Early return in case of cache hit
    if config::enable_cache() {
        if let Some(mut result) = cache.get(hash) {
            info!(
                "Using cached result {:?} for program {}",
                &result,
                request.program.get_name()
            );
            if config::dump_viper_program() {
                ast_utils.with_local_frame(16, || {
                    let _ = build_or_dump_viper_program();
                });
            }
            normalization_info.denormalize_result(&mut result);
            sender.send(ServerMessage::Termination(result)).unwrap();
            return;
        }
    };

    ast_utils.with_local_frame(16, || {
        let viper_program = build_or_dump_viper_program();
        let program_name = request.program.get_name();

        // Create a new verifier each time.
        // Workaround for https://github.com/viperproject/prusti-dev/issues/744
        let mut stopwatch = Stopwatch::start("prusti-server", "verifier startup");
        let mut verifier =
            new_viper_verifier(program_name, &verification_context, request.backend_config);

        let mut result = VerificationResult::Success;
        let normalization_info_clone = normalization_info.clone();
        let sender_clone = sender.clone();

        // start thread for polling messages and print on receive
        // TODO: Detach warning
        thread::scope(|scope| {
            // get the reporter
            let env = &verification_context.env();
            let jni = JniUtils::new(env);
            let verifier_wrapper = silver::verifier::Verifier::with(env);
            let reporter = jni.unwrap_result(verifier_wrapper.call_reporter(verifier.verifier_instance().clone()));
            let rep_glob_ref = env.new_global_ref(reporter).unwrap();

            let (main_tx, thread_rx) = mpsc::channel();
            let polling_thread = scope.spawn(move || {
                let verification_context = viper_arc.attach_current_thread();
                let env = verification_context.env();
                let jni = JniUtils::new(env);
                let reporter_instance = rep_glob_ref.as_obj();
                let reporter_wrapper = silver::reporter::PollingReporter::with(env);
                let mut done = false;
                while !done {
                    while reporter_wrapper.call_hasNewMessage(reporter_instance).unwrap() {
                        let msg = reporter_wrapper.call_getNewMessage(reporter_instance).unwrap();
                        match jni.class_name(msg).as_str() {
                            "viper.silver.reporter.QuantifierInstantiationsMessage" => {
                                let msg_wrapper = silver::reporter::QuantifierInstantiationsMessage::with(env);
                                let q_name = jni.get_string(jni.unwrap_result(msg_wrapper.call_quantifier(msg)));
                                let q_inst = jni.unwrap_result(msg_wrapper.call_instantiations(msg));
                                // TODO: find out which more quantifiers are derived from the user
                                // quantifiers
                                info!("QuantifierInstantiationsMessage: {} {}", q_name, q_inst);
                                // also matches the "-aux" quantifiers generated
                                // TODO: some positions have just the id 0 and cannot be denormalized...
                                if q_name.starts_with("quant_with_posID") {
                                    let no_pref = q_name.strip_prefix("quant_with_posID").unwrap();
                                    let stripped = no_pref.strip_suffix("-aux").or(Some(no_pref)).unwrap();
                                    let parsed = stripped.parse::<u64>();
                                    match parsed {
                                        Ok(pos_id) => {
                                            let norm_pos_id = normalization_info_clone.denormalize_position_id(pos_id);
                                            sender_clone.send(ServerMessage::QuantifierInstantiation{q_name: q_name, insts: u64::try_from(q_inst).unwrap(), norm_pos_id: norm_pos_id}).unwrap();
                                        }
                                        _ => info!("Unexpected quantifier name {}", q_name)
                                    }
                                }
                            }
                            _ => ()
                        }
                    }
                    if !thread_rx.try_recv().is_err() {
                        info!("Polling thread received termination signal!");
                        done = true;
                    } else {
                        thread::sleep(time::Duration::from_millis(10));
                    }
                }
            });
            stopwatch.start_next("verification");
            result = verifier.verify(viper_program);
            // send termination signal to polling thread
            main_tx.send(()).unwrap();
            // FIXME: here the global ref is dropped from a detached thread
            polling_thread.join().unwrap();
        });

        // Don't cache Java exceptions, which might be due to misconfigured paths.
        if config::enable_cache() && !matches!(result, VerificationResult::JavaException(_)) {
            info!(
                "Storing new cached result {:?} for program {}",
                &result,
                request.program.get_name()
            );
            cache.insert(hash, result.clone());
        }

        normalization_info.denormalize_result(&mut result);
        sender.send(ServerMessage::Termination(result)).unwrap();
    })
}

fn dump_viper_program(ast_utils: &viper::AstUtils, program: viper::Program, program_name: &str) {
    let namespace = "viper_program";
    let filename = format!("{program_name}.vpr");
    info!("Dumping Viper program to '{}/{}'", namespace, filename);
    report(namespace, filename, ast_utils.pretty_print(program));
}

fn new_viper_verifier<'v, 't: 'v>(
    program_name: &str,
    verification_context: &'v viper::VerificationContext<'t>,
    backend_config: ViperBackendConfig,
) -> viper::Verifier<'v> {
    let mut verifier_args: Vec<String> = backend_config.verifier_args;
    let report_path: Option<PathBuf>;
    if config::dump_debug_info() {
        let log_path = config::log_dir()
            .join("viper_tmp")
            .join(to_legal_file_name(program_name));
        create_dir_all(&log_path).unwrap();
        report_path = Some(log_path.join("report.csv"));
        let log_dir_str = log_path.to_str().unwrap();
        match backend_config.backend {
            VerificationBackend::Silicon => {
                verifier_args.extend(vec![
                    "--tempDirectory".to_string(),
                    log_dir_str.to_string(),
                    "--printMethodCFGs".to_string(),
                    //"--printTranslatedProgram".to_string(),
                ])
            }
            VerificationBackend::Carbon => verifier_args.extend(vec![
                "--boogieOpt".to_string(),
                format!("/logPrefix {log_dir_str}"),
                //"--print".to_string(), "./log/boogie_program/program.bpl".to_string(),
            ]),
        }
    } else {
        report_path = None;
        if backend_config.backend == VerificationBackend::Silicon {
            verifier_args.extend(vec!["--disableTempDirectory".to_string()]);
        }
    }
    let (smt_solver, smt_manager) = if config::use_smt_wrapper() {
        std::env::set_var("PRUSTI_ORIGINAL_SMT_SOLVER_PATH", config::smt_solver_path());
        let log_path = config::log_dir()
            .join("smt")
            .join(to_legal_file_name(program_name));
        create_dir_all(&log_path).unwrap();
        let smt_manager = SmtManager::new(
            log_path,
            config::preserve_smt_trace_files(),
            config::write_smt_statistics(),
            config::smt_qi_ignore_builtin(),
            config::smt_qi_bound_global_kind(),
            config::smt_qi_bound_trace(),
            config::smt_qi_bound_trace_kind(),
            config::smt_unique_triggers_bound(),
            config::smt_unique_triggers_bound_total(),
        );
        std::env::set_var(
            "PRUSTI_SMT_SOLVER_MANAGER_PORT",
            smt_manager.port().to_string(),
        );
        if config::log_smt_wrapper_interaction() {
            std::env::set_var("PRUSTI_LOG_SMT_INTERACTION", "true");
        }
        (config::smt_solver_wrapper_path(), smt_manager)
    } else {
        (config::smt_solver_path(), SmtManager::default())
    };
    let boogie_path = config::boogie_path();
    if let Some(bound) = config::smt_qi_bound_global() {
        // We need to set the environment variable to reach our Z3 wrapper.
        std::env::set_var("PRUSTI_SMT_QI_BOUND_GLOBAL", bound.to_string());
    }

    verification_context.new_verifier(
        backend_config.backend,
        verifier_args,
        report_path,
        smt_solver,
        boogie_path,
        smt_manager,
    )
}
