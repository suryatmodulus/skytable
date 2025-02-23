/*
 * Created on Wed Jun 16 2021
 *
 * This file is a part of Skytable
 * Skytable (formerly known as TerrabaseDB or Skybase) is a free and open-source
 * NoSQL database written by Sayan Nandan ("the Author") with the
 * vision to provide flexibility in data modelling without compromising
 * on performance, queryability or scalability.
 *
 * Copyright (c) 2021, Sayan Nandan <ohsayan@outlook.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program. If not, see <https://www.gnu.org/licenses/>.
 *
*/

//! # libstress
//!
//! Tools for emulating concurrent query behavior to _stress test_ the database server.
//! As of now, this crate provides a [`Workpool`] which is a generic synchronous threadpool
//! for doing multiple operations. But Workpool is a little different from standard threadpool
//! implementations in it categorizing a job to be made up of three parts, namely:
//!
//! - The init_pre_loop_var (the pre-loop stage)
//! - The on_loop (the in-loop stage)
//! - The on_exit (the post-loop stage)
//!
//! These stages form a part of the event loop.
//!
//! ## The event loop
//!
//! A task runs in a loop with the `on_loop` routine to which the a reference of the result of
//! the `init_pre_loop_var` is sent that is initialized. The loop proceeds whenever a worker
//! receives a task or else it blocks the current thread, waiting for a task. Hence the loop
//! cannot be terminated by an execute call. Instead, the _event loop_ is terminated when the
//! Workpool is dropped, either by scoping out, or by using the provided finish-like methods
//! (that call the destructor).
//!
//! ## Worker lifetime
//!
//! If a runtime panic occurs in the pre-loop stage, then the entire worker just terminates. Hence
//! this worker is no longer able to perform any tasks. Similarly, if a runtime panic occurs in
//! the in-loop stage, the worker terminates and is no longer available to do any work. This will
//! be reflected when the workpool attempts to terminate in entirety, i.e when the threads are joined
//! to the parent thread
//!

#![deny(unused_crate_dependencies)]
#![deny(unused_imports)]

pub mod traits;
pub use rayon;

use {
    core::marker::PhantomData,
    crossbeam_channel::{bounded, unbounded, Receiver as CReceiver, Sender as CSender},
    rayon::prelude::{IntoParallelIterator, ParallelIterator},
    std::{fmt::Display, thread},
};

#[derive(Debug)]
pub enum WorkpoolError {
    ThreadStartFailure(usize, usize),
}

impl Display for WorkpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkpoolError::ThreadStartFailure(expected, started) => {
                write!(
                    f,
                    "couldn't start all threads. expected {expected} but started {started}"
                )
            }
        }
    }
}

pub type WorkpoolResult<T> = Result<T, WorkpoolError>;

/// A Job. The UIn type parameter is the type that will be used to execute the action
/// Nothing is a variant used by the drop implementation to terminate all the workers
/// and call the exit_loop function
enum JobType<UIn> {
    Task(UIn),
    Nothing,
}

/// A worker
///
/// The only reason we use option is to reduce the effort needed to implement [`Drop`] for the
/// [`Workpool`]
struct Worker {
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Initialize a new worker
    fn new<Inp: 'static, UIn, Lv, Lp, Ex>(
        id: usize,
        job_receiver: CReceiver<JobType<UIn>>,
        init_pre_loop_var: Lv,
        on_exit: Ex,
        on_loop: Lp,
        wgtx: CSender<()>,
    ) -> Self
    where
        UIn: Send + Sync + 'static,
        Lv: Fn() -> Inp + 'static + Send,
        Lp: Fn(&mut Inp, UIn) + Send + Sync + 'static,
        Ex: Fn(&mut Inp) + Send + 'static,
    {
        let thread = thread::Builder::new()
            .name(format!("worker-{id}"))
            .spawn(move || {
                let on_loop = on_loop;
                let mut pre_loop_var = init_pre_loop_var();
                wgtx.send(()).unwrap();
                drop(wgtx);
                loop {
                    let action = job_receiver.recv().unwrap();
                    match action {
                        JobType::Task(tsk) => on_loop(&mut pre_loop_var, tsk),
                        JobType::Nothing => {
                            on_exit(&mut pre_loop_var);
                            break;
                        }
                    }
                }
            })
            .unwrap();
        Self {
            thread: Some(thread),
        }
    }
}

/// A pool configuration setting to easily generate [`Workpool`]s without
/// having to clone an entire pool and its threads upfront
pub struct PoolConfig<Inp, UIn, Lv, Lp, Ex> {
    /// the pool size
    count: usize,
    /// the function that sets the pre-loop variable
    init_pre_loop_var: Lv,
    /// the function to be executed on worker termination
    on_exit: Ex,
    /// the function to be executed on loop
    on_loop: Lp,
    /// a marker for `Inp` since no parameters use it directly
    _marker: PhantomData<(Inp, UIn)>,
    /// check if self needs a pool for parallel iterators
    needs_iterator_pool: bool,
    /// expected maximum number of jobs
    expected_max_sends: Option<usize>,
}

impl<Inp: 'static, UIn, Lv, Lp, Ex> PoolConfig<Inp, UIn, Lv, Lp, Ex>
where
    UIn: Send + Sync + 'static,
    Inp: Sync,
    Ex: Fn(&mut Inp) + Send + Sync + 'static + Clone,
    Lv: Fn() -> Inp + Send + Sync + 'static + Clone,
    Lp: Fn(&mut Inp, UIn) + Clone + Send + Sync + 'static,
{
    /// Create a new pool config
    pub fn new(
        count: usize,
        init_pre_loop_var: Lv,
        on_loop: Lp,
        on_exit: Ex,
        needs_iterator_pool: bool,
        expected_max_sends: Option<usize>,
    ) -> Self {
        Self {
            count,
            init_pre_loop_var,
            on_loop,
            on_exit,
            needs_iterator_pool,
            _marker: PhantomData,
            expected_max_sends,
        }
    }
    /// Get a new [`Workpool`] from the current config
    pub fn get_pool(&self) -> WorkpoolResult<Workpool<Inp, UIn, Lv, Lp, Ex>> {
        self.get_pool_with_workers(self.count)
    }
    /// Get a [`Workpool`] with the base config but with a different number of workers
    pub fn get_pool_with_workers(
        &self,
        count: usize,
    ) -> WorkpoolResult<Workpool<Inp, UIn, Lv, Lp, Ex>> {
        Workpool::new(
            count,
            self.init_pre_loop_var.clone(),
            self.on_loop.clone(),
            self.on_exit.clone(),
            self.needs_iterator_pool,
            self.expected_max_sends,
        )
    }
    /// Get a [`Workpool`] with the base config but with a custom loop-stage closure
    pub fn with_loop_closure<Dlp>(&self, lp: Dlp) -> WorkpoolResult<Workpool<Inp, UIn, Lv, Dlp, Ex>>
    where
        Dlp: Fn(&mut Inp, UIn) + Clone + Send + Sync + 'static,
    {
        Workpool::new(
            self.count,
            self.init_pre_loop_var.clone(),
            lp,
            self.on_exit.clone(),
            self.needs_iterator_pool,
            self.expected_max_sends,
        )
    }
}

/// # Workpool
///
/// A Workpool is a generic synchronous thread pool that can be used to perform, well, anything.
/// A workpool has to be initialized with the number of workers, the pre_loop_variable (set this
/// to None if there isn't any). what to do on loop and what to do on exit of each worker. The
/// closures are kept as `Clone`able types just to reduce complexity with copy (we were lazy).
///
/// ## Clones
///
/// Workpool clones simply create a new workpool with the same on_exit, on_loop and init_pre_loop_var
/// configurations. This provides a very convenient interface if one desires to use multiple workpools
/// to do the _same kind of thing_
///
/// ## Actual thread count
///
/// The actual thread count will depend on whether the caller requests the initialization of an
/// iterator pool or not. If the caller does request for an iterator pool, then the number of threads
/// spawned will be twice the number of the set workers. Else, the number of spawned threads is equal
/// to the number of workers.
pub struct Workpool<Inp, UIn, Lv, Lp, Ex> {
    /// the workers
    workers: Vec<Worker>,
    /// the sender that sends jobs
    job_distributor: CSender<JobType<UIn>>,
    /// the function that sets the pre-loop variable
    init_pre_loop_var: Lv,
    /// the function to be executed on worker termination
    on_exit: Ex,
    /// the function to be executed on loop
    on_loop: Lp,
    /// a marker for `Inp` since no parameters use it directly
    _marker: PhantomData<Inp>,
    /// check if self needs a pool for parallel iterators
    needs_iterator_pool: bool,
    /// expected maximum number of sends
    expected_max_sends: Option<usize>,
}

impl<Inp: 'static, UIn, Lv, Ex, Lp> Workpool<Inp, UIn, Lv, Lp, Ex>
where
    UIn: Send + Sync + 'static,
    Ex: Fn(&mut Inp) + Send + Sync + 'static + Clone,
    Lv: Fn() -> Inp + Send + Sync + 'static + Clone,
    Lp: Fn(&mut Inp, UIn) + Send + Sync + 'static + Clone,
    Inp: Sync,
{
    /// Create a new workpool
    pub fn new(
        count: usize,
        init_pre_loop_var: Lv,
        on_loop: Lp,
        on_exit: Ex,
        needs_iterator_pool: bool,
        expected_max_sends: Option<usize>,
    ) -> WorkpoolResult<Self> {
        // init threadpool for iterator
        if needs_iterator_pool {
            // initialize a global threadpool for parallel iterators
            let _ = rayon::ThreadPoolBuilder::new()
                .num_threads(count)
                .build_global();
        }
        assert!(count != 0, "Runtime panic: Bad value `0` for thread count");
        let (sender, receiver) = match expected_max_sends {
            Some(limit) => bounded(limit),
            None => unbounded(),
        };
        let (wgtx, wgrx) = bounded::<()>(count);
        let mut workers = Vec::with_capacity(count);
        for i in 0..count {
            workers.push(Worker::new(
                i,
                receiver.clone(),
                init_pre_loop_var.clone(),
                on_exit.clone(),
                on_loop.clone(),
                wgtx.clone(),
            ));
        }
        drop(wgtx);
        let sum: usize = wgrx.iter().map(|_| 1usize).sum();
        if sum == count {
            Ok(Self {
                workers,
                job_distributor: sender,
                init_pre_loop_var,
                on_exit,
                on_loop,
                _marker: PhantomData,
                needs_iterator_pool,
                expected_max_sends,
            })
        } else {
            Err(WorkpoolError::ThreadStartFailure(count, sum))
        }
    }
    pub fn clone_pool(&self) -> WorkpoolResult<Self> {
        Self::new(
            self.workers.len(),
            self.init_pre_loop_var.clone(),
            self.on_loop.clone(),
            self.on_exit.clone(),
            self.needs_iterator_pool,
            self.expected_max_sends,
        )
    }
    /// Execute something
    pub fn execute(&self, inp: UIn) {
        self.job_distributor
            .send(JobType::Task(inp))
            .expect("Worker thread crashed")
    }
    /// Execute something that can be executed as a parallel iterator
    /// For the best performance, it is recommended that you pass true for `needs_iterator_pool`
    /// on initialization of the [`Workpool`]
    pub fn execute_iter(&self, iter: impl IntoParallelIterator<Item = UIn>) {
        iter.into_par_iter().for_each(|inp| self.execute(inp))
    }
    /// Does the same thing as [`execute_iter`] but drops self ensuring that all the
    /// workers actually finish their tasks
    pub fn execute_and_finish_iter(self, iter: impl IntoParallelIterator<Item = UIn>) {
        self.execute_iter(iter);
        drop(self);
    }
    /// Initialize a new [`Workpool`] with the default count of threads. This is equal
    /// to 2 * the number of logical cores.
    pub fn new_default_threads(
        init_pre_loop_var: Lv,
        on_loop: Lp,
        on_exit: Ex,
        needs_iterator_pool: bool,
        expected_max_sends: Option<usize>,
    ) -> WorkpoolResult<Self> {
        // we'll naively use the number of CPUs present on the system times 2 to determine
        // the number of workers (sure the scheduler does tricks all the time)
        let worker_count = thread::available_parallelism().map_or(1, usize::from) * 2;
        Self::new(
            worker_count,
            init_pre_loop_var,
            on_loop,
            on_exit,
            needs_iterator_pool,
            expected_max_sends,
        )
    }
}

impl<Inp, UIn, Lv, Lp, Ex> Drop for Workpool<Inp, UIn, Lp, Lv, Ex> {
    fn drop(&mut self) {
        for _ in &self.workers {
            self.job_distributor.send(JobType::Nothing).unwrap();
        }
        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap()
            }
        }
    }
}

pub mod utils {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    use rand::distributions::{Alphanumeric, Standard};
    use std::collections::HashSet;
    use std::collections::TryReserveError;

    /// Generate a random UTF-8 string
    pub fn ran_string(len: usize, rand: impl rand::Rng) -> String {
        let rand_string: String = rand
            .sample_iter(&Alphanumeric)
            .take(len)
            .map(char::from)
            .collect();
        rand_string
    }
    /// Generate a vector of random bytes
    pub fn ran_bytes(len: usize, rand: impl rand::Rng) -> Vec<u8> {
        rand.sample_iter(&Standard).take(len).collect()
    }
    /// Generate multiple vectors of random bytes
    pub fn generate_random_byte_vector(
        count: usize,
        size: usize,
        mut rng: impl rand::Rng,
        unique: bool,
    ) -> Result<Vec<Vec<u8>>, TryReserveError> {
        if unique {
            let mut keys = HashSet::new();
            keys.try_reserve(size)?;
            (0..count).into_iter().for_each(|_| {
                let mut ran = ran_bytes(size, &mut rng);
                while keys.contains(&ran) {
                    ran = ran_bytes(size, &mut rng);
                }
                keys.insert(ran);
            });
            Ok(keys.into_iter().collect())
        } else {
            let mut keys = Vec::new();
            keys.try_reserve_exact(size)?;
            let ran_byte_key = ran_bytes(size, &mut rng);
            (0..count).for_each(|_| keys.push(ran_byte_key.clone()));
            Ok(keys)
        }
    }
    /// Generate a vector of random UTF-8 valid strings
    pub fn generate_random_string_vector(
        count: usize,
        size: usize,
        mut rng: impl rand::Rng,
        unique: bool,
    ) -> Result<Vec<String>, TryReserveError> {
        if unique {
            let mut keys = HashSet::new();
            keys.try_reserve(size)?;
            (0..count).into_iter().for_each(|_| {
                let mut ran = ran_string(size, &mut rng);
                while keys.contains(&ran) {
                    ran = ran_string(size, &mut rng);
                }
                keys.insert(ran);
            });
            Ok(keys.into_iter().collect())
        } else {
            let mut keys = Vec::new();
            keys.try_reserve_exact(size)?;
            (0..count)
                .into_iter()
                .map(|_| ran_string(size, &mut rng))
                .for_each(|bytes| keys.push(bytes));
            Ok(keys)
        }
    }
    pub fn rand_alphastring(len: usize, rng: &mut impl rand::Rng) -> String {
        (0..len)
            .map(|_| {
                let idx = rng.gen_range(0..CHARSET.len());
                CHARSET[idx] as char
            })
            .collect()
    }
}
