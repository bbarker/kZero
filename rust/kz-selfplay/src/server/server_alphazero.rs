use std::sync::Arc;

use board_game::board::Board;
use crossbeam::thread::Scope;
use flume::Sender;
use futures::executor::ThreadPoolBuilder;

use cuda_sys::wrapper::handle::Device;
use kz_core::mapping::BoardMapper;
use nn_graph::graph::Graph;
use nn_graph::onnx::load_graph_from_onnx_path;
use nn_graph::optimizer::optimize_graph;

use crate::server::executor::executor_loop_alphazero;
use crate::server::generator_alphazero::generator_alphazero_main;
use crate::server::job_channel::job_pair;
use crate::server::protocol::{GeneratorUpdate, Settings, StartupSettings};
use crate::server::server::ZeroSpecialization;

#[derive(Debug)]
pub struct AlphaZeroSpecialization;

impl<B: Board, M: BoardMapper<B> + 'static> ZeroSpecialization<B, M> for AlphaZeroSpecialization {
    type G = Graph;

    fn spawn_device_threads<'s>(
        &self,
        s: &Scope<'s>,
        device: Device,
        device_id: usize,
        startup: &StartupSettings,
        mapper: M,
        start_pos: impl Fn() -> B + Send + Sync + Clone + 'static,
        update_sender: Sender<GeneratorUpdate<B>>,
    ) -> (Vec<Sender<Settings>>, Vec<Sender<Arc<Graph>>>) {
        let gpu_batch_size = startup.gpu_batch_size;
        let cpu_threads = startup.cpu_threads_per_device;
        let gpu_threads = startup.gpu_threads_per_device;
        let concurrent_games = (gpu_threads + 1) * gpu_batch_size;
        println!("Running {} concurrent games", concurrent_games);

        let mut settings_senders: Vec<Sender<Settings>> = vec![];
        let mut graph_senders: Vec<Sender<Arc<Graph>>> = vec![];

        let (eval_client, eval_server) = job_pair(gpu_threads * gpu_batch_size);

        // spawn cpu threads
        let pool = ThreadPoolBuilder::new()
            .pool_size(cpu_threads)
            .name_prefix(format!("generator-{}-", device_id))
            .create()
            .unwrap();

        for generator_id in 0..concurrent_games {
            let start_pos = start_pos.clone();
            let eval_client = eval_client.clone();
            let update_sender = update_sender.clone();

            let (settings_sender, settings_receiver) = flume::bounded(1);
            settings_senders.push(settings_sender);

            pool.spawn_ok(async move {
                generator_alphazero_main(generator_id, start_pos, settings_receiver, eval_client, update_sender).await;
            });
        }

        // spawn gpu eval threads
        for local_id in 0..gpu_threads {
            let (graph_sender, graph_receiver) = flume::bounded(1);
            graph_senders.push(graph_sender);

            let eval_server = eval_server.clone();

            s.builder()
                .name(format!("gpu-expand-{}-{}", device_id, local_id))
                .spawn(move |_| {
                    executor_loop_alphazero(device, gpu_batch_size, mapper, graph_receiver, eval_server);
                })
                .unwrap();
        }

        (settings_senders, graph_senders)
    }

    fn load_graph(&self, path: &str, _: M) -> Self::G {
        optimize_graph(&load_graph_from_onnx_path(path), Default::default())
    }
}
