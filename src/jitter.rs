// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// https://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// Based on jitterentropy-library, http://www.chronox.de/jent.html.
// Copyright Stephan Mueller <smueller@chronox.de>, 2014 - 2017.
//
// With permission from Stephan Mueller to relicense the Rust translation under
// the MIT license.

//! Non-physical true random number generator based on timing jitter.

// Note: the C implementation of `Jitterentropy` relies on being compiled
// without optimizations. This implementation goes through lengths to make the
// compiler not optimise out what is technically dead code, but that does
// influence timing jitter.

use rand_core::{RngCore, CryptoRng, Error, ErrorKind, impls};

use core::{fmt, mem, ptr};
#[cfg(feature="std")]
use std::sync::atomic::{AtomicUsize, ATOMIC_USIZE_INIT, Ordering};

const MEMORY_BLOCKS: usize = 64;
const MEMORY_BLOCKSIZE: usize = 32;
const MEMORY_SIZE: usize = MEMORY_BLOCKS * MEMORY_BLOCKSIZE;

/// A true random number generator based on jitter in the CPU execution time,
/// and jitter in memory access time.
///
/// This is a true random number generator, as opposed to pseudo-random
/// generators. Random numbers generated by `JitterRng` can be seen as fresh
/// entropy. A consequence is that is orders of magnitude slower than [`OsRng`]
/// and PRNGs (about 10<sup>3</sup>..10<sup>6</sup> slower).
///
/// There are very few situations where using this RNG is appropriate. Only very
/// few applications require true entropy. A normal PRNG can be statistically
/// indistinguishable, and a cryptographic PRNG should also be as impossible to
/// predict.
///
/// Use of `JitterRng` is recommended for initializing cryptographic PRNGs when
/// [`OsRng`] is not available.
///
/// This implementation is based on
/// [Jitterentropy](http://www.chronox.de/jent.html) version 2.1.0.
///
/// [`OsRng`]: ../os/struct.OsRng.html
pub struct JitterRng {
    data: u64, // Actual random number
    // Number of rounds to run the entropy collector per 64 bits
    rounds: u8,
    // Timer used by `measure_jitter`
    timer: fn() -> u64,
    // Memory for the Memory Access noise source
    mem_prev_index: u16,
    // Make `next_u32` not waste 32 bits
    data_half_used: bool,
}

// Entropy collector state.
// These values are not necessary to preserve across runs.
struct EcState {
    // Previous time stamp to determine the timer delta
    prev_time: u64,
    // Deltas used for the stuck test
    last_delta: i32,
    last_delta2: i32,
    // Memory for the Memory Access noise source
    mem: [u8; MEMORY_SIZE],
}

impl EcState {
    // Stuck test by checking the:
    // - 1st derivation of the jitter measurement (time delta)
    // - 2nd derivation of the jitter measurement (delta of time deltas)
    // - 3rd derivation of the jitter measurement (delta of delta of time
    //   deltas)
    //
    // All values must always be non-zero.
    // This test is a heuristic to see whether the last measurement holds
    // entropy.
    fn stuck(&mut self, current_delta: i32) -> bool {
        let delta2 = self.last_delta - current_delta;
        let delta3 = delta2 - self.last_delta2;

        self.last_delta = current_delta;
        self.last_delta2 = delta2;

        current_delta == 0 || delta2 == 0 || delta3 == 0
    }
}

// Custom Debug implementation that does not expose the internal state
impl fmt::Debug for JitterRng {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "JitterRng {{}}")
    }
}

/// An error that can occur when [`JitterRng::test_timer`] fails.
///
/// [`JitterRng::test_timer`]: struct.JitterRng.html#method.test_timer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerError {
    /// No timer available.
    NoTimer,
    /// Timer too coarse to use as an entropy source.
    CoarseTimer,
    /// Timer is not monotonically increasing.
    NotMonotonic,
    /// Variations of deltas of time too small.
    TinyVariantions,
    /// Too many stuck results (indicating no added entropy).
    TooManyStuck,
    #[doc(hidden)]
    __Nonexhaustive,
}

impl TimerError {
    fn description(&self) -> &'static str {
        match *self {
            TimerError::NoTimer => "no timer available",
            TimerError::CoarseTimer => "coarse timer",
            TimerError::NotMonotonic => "timer not monotonic",
            TimerError::TinyVariantions => "time delta variations too small",
            TimerError::TooManyStuck => "too many stuck results",
            TimerError::__Nonexhaustive => unreachable!(),
        }
    }
}

impl fmt::Display for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.description())
    }
}

#[cfg(feature="std")]
impl ::std::error::Error for TimerError {
    fn description(&self) -> &str {
        self.description()
    }
}

impl From<TimerError> for Error {
    fn from(err: TimerError) -> Error {
        // Timer check is already quite permissive of failures so we don't
        // expect false-positive failures, i.e. any error is irrecoverable.
        Error::with_cause(ErrorKind::Unavailable,
                              "timer jitter failed basic quality tests", err)
    }
}

// Initialise to zero; must be positive
#[cfg(feature="std")]
static JITTER_ROUNDS: AtomicUsize = ATOMIC_USIZE_INIT;

impl JitterRng {
    /// Create a new `JitterRng`. Makes use of `std::time` for a timer, or a
    /// platform-specific function with higher accuracy if necessary and
    /// available.
    ///
    /// During initialization CPU execution timing jitter is measured a few
    /// hundred times. If this does not pass basic quality tests, an error is
    /// returned. The test result is cached to make subsequent calls faster.
    #[cfg(feature="std")]
    pub fn new() -> Result<JitterRng, TimerError> {
        let mut state = JitterRng::new_with_timer(platform::get_nstime);
        let mut rounds = JITTER_ROUNDS.load(Ordering::Relaxed) as u8;
        if rounds == 0 {
            // No result yet: run test.
            // This allows the timer test to run multiple times; we don't care.
            rounds = state.test_timer()?;
            JITTER_ROUNDS.store(rounds as usize, Ordering::Relaxed);
            info!("JitterRng: using {} rounds per u64 output", rounds);
        }
        state.set_rounds(rounds);

        // Fill `data` with a non-zero value.
        state.gen_entropy();
        Ok(state)
    }

    /// Create a new `JitterRng`.
    /// A custom timer can be supplied, making it possible to use `JitterRng` in
    /// `no_std` environments.
    ///
    /// The timer must have nanosecond precision.
    ///
    /// This method is more low-level than `new()`. It is the responsibility of
    /// the caller to run [`test_timer`] before using any numbers generated with
    /// `JitterRng`, and optionally call [`set_rounds`]. Also it is important to
    /// consume at least one `u64` before using the first result to initialize
    /// the entropy collection pool.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use rand::{Rng, Error};
    /// use rand::JitterRng;
    ///
    /// # fn try_inner() -> Result<(), Error> {
    /// fn get_nstime() -> u64 {
    ///     use std::time::{SystemTime, UNIX_EPOCH};
    ///
    ///     let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    ///     // The correct way to calculate the current time is
    ///     // `dur.as_secs() * 1_000_000_000 + dur.subsec_nanos() as u64`
    ///     // But this is faster, and the difference in terms of entropy is
    ///     // negligible (log2(10^9) == 29.9).
    ///     dur.as_secs() << 30 | dur.subsec_nanos() as u64
    /// }
    ///
    /// let mut rng = JitterRng::new_with_timer(get_nstime);
    /// let rounds = rng.test_timer()?;
    /// rng.set_rounds(rounds); // optional
    /// let _ = rng.gen::<u64>();
    ///
    /// // Ready for use
    /// let v: u64 = rng.gen();
    /// # Ok(())
    /// # }
    ///
    /// # let _ = try_inner();
    /// ```
    ///
    /// [`test_timer`]: struct.JitterRng.html#method.test_timer
    /// [`set_rounds`]: struct.JitterRng.html#method.set_rounds
    pub fn new_with_timer(timer: fn() -> u64) -> JitterRng {
        JitterRng {
            data: 0,
            rounds: 64,
            timer,
            mem_prev_index: 0,
            data_half_used: false,
        }
    }

    /// Configures how many rounds are used to generate each 64-bit value.
    /// This must be greater than zero, and has a big impact on performance
    /// and output quality.
    ///
    /// [`new_with_timer`] conservatively uses 64 rounds, but often less rounds
    /// can be used. The `test_timer()` function returns the minimum number of
    /// rounds required for full strength (platform dependent), so one may use
    /// `rng.set_rounds(rng.test_timer()?);` or cache the value.
    ///
    /// [`new_with_timer`]: struct.JitterRng.html#method.new_with_timer
    pub fn set_rounds(&mut self, rounds: u8) {
        assert!(rounds > 0);
        self.rounds = rounds;
    }

    // Calculate a random loop count used for the next round of an entropy
    // collection, based on bits from a fresh value from the timer.
    //
    // The timer is folded to produce a number that contains at most `n_bits`
    // bits.
    //
    // Note: A constant should be added to the resulting random loop count to
    // prevent loops that run 0 times.
    #[inline(never)]
    fn random_loop_cnt(&mut self, n_bits: u32) -> u32 {
        let mut rounds = 0;

        let mut time = (self.timer)();
        // Mix with the current state of the random number balance the random
        // loop counter a bit more.
        time ^= self.data;

        // We fold the time value as much as possible to ensure that as many
        // bits of the time stamp are included as possible.
        let folds = (64 + n_bits - 1) / n_bits;
        let mask = (1 << n_bits) - 1;
        for _ in 0..folds {
            rounds ^= time & mask;
            time >>= n_bits;
        }

        rounds as u32
    }

    // CPU jitter noise source
    // Noise source based on the CPU execution time jitter
    //
    // This function injects the individual bits of the time value into the
    // entropy pool using an LFSR.
    //
    // The code is deliberately inefficient with respect to the bit shifting.
    // This function not only acts as folding operation, but this function's
    // execution is used to measure the CPU execution time jitter. Any change to
    // the loop in this function implies that careful retesting must be done.
    #[inline(never)]
    fn lfsr_time(&mut self, time: u64, var_rounds: bool) {
        fn lfsr(mut data: u64, time: u64) -> u64{
            for i in 1..65 {
                let mut tmp = time << (64 - i);
                tmp >>= 64 - 1;

                // Fibonacci LSFR with polynomial of
                // x^64 + x^61 + x^56 + x^31 + x^28 + x^23 + 1 which is
                // primitive according to
                // http://poincare.matf.bg.ac.rs/~ezivkovm/publications/primpol1.pdf
                // (the shift values are the polynomial values minus one
                // due to counting bits from 0 to 63). As the current
                // position is always the LSB, the polynomial only needs
                // to shift data in from the left without wrap.
                data ^= tmp;
                data ^= (data >> 63) & 1;
                data ^= (data >> 60) & 1;
                data ^= (data >> 55) & 1;
                data ^= (data >> 30) & 1;
                data ^= (data >> 27) & 1;
                data ^= (data >> 22) & 1;
                data = data.rotate_left(1);
            }
            data
        }

        // Note: in the reference implementation only the last round effects
        // `self.data`, all the other results are ignored. To make sure the
        // other rounds are not optimised out, we first run all but the last
        // round on a throw-away value instead of the real `self.data`.
        let mut lfsr_loop_cnt = 0;
        if var_rounds { lfsr_loop_cnt = self.random_loop_cnt(4) };

        let mut throw_away: u64 = 0;
        for _ in 0..lfsr_loop_cnt {
            throw_away = lfsr(throw_away, time);
        }
        black_box(throw_away);

        self.data = lfsr(self.data, time);
    }

    // Memory Access noise source
    // This is a noise source based on variations in memory access times
    //
    // This function performs memory accesses which will add to the timing
    // variations due to an unknown amount of CPU wait states that need to be
    // added when accessing memory. The memory size should be larger than the L1
    // caches as outlined in the documentation and the associated testing.
    //
    // The L1 cache has a very high bandwidth, albeit its access rate is usually
    // slower than accessing CPU registers. Therefore, L1 accesses only add
    // minimal variations as the CPU has hardly to wait. Starting with L2,
    // significant variations are added because L2 typically does not belong to
    // the CPU any more and therefore a wider range of CPU wait states is
    // necessary for accesses. L3 and real memory accesses have even a wider
    // range of wait states. However, to reliably access either L3 or memory,
    // the `self.mem` memory must be quite large which is usually not desirable.
    #[inline(never)]
    fn memaccess(&mut self, mem: &mut [u8; MEMORY_SIZE], var_rounds: bool) {
        let mut acc_loop_cnt = 128;
        if var_rounds { acc_loop_cnt += self.random_loop_cnt(4) };

        let mut index = self.mem_prev_index as usize;
        for _ in 0..acc_loop_cnt {
            // Addition of memblocksize - 1 to index with wrap around logic to
            // ensure that every memory location is hit evenly.
            // The modulus also allows the compiler to remove the indexing
            // bounds check.
            index = (index + MEMORY_BLOCKSIZE - 1) % MEMORY_SIZE;

            // memory access: just add 1 to one byte
            // memory access implies read from and write to memory location
            mem[index] = mem[index].wrapping_add(1);
        }
        self.mem_prev_index = index as u16;
    }

    // This is the heart of the entropy generation: calculate time deltas and
    // use the CPU jitter in the time deltas. The jitter is injected into the
    // entropy pool.
    //
    // Ensure that `ec.prev_time` is primed before using the output of this
    // function. This can be done by calling this function and not using its
    // result.
    fn measure_jitter(&mut self, ec: &mut EcState) -> Option<()> {
        // Invoke one noise source before time measurement to add variations
        self.memaccess(&mut ec.mem, true);

        // Get time stamp and calculate time delta to previous
        // invocation to measure the timing variations
        let time = (self.timer)();
        // Note: wrapping_sub combined with a cast to `i64` generates a correct
        // delta, even in the unlikely case this is a timer that is not strictly
        // monotonic.
        let current_delta = time.wrapping_sub(ec.prev_time) as i64 as i32;
        ec.prev_time = time;

        // Call the next noise source which also injects the data
        self.lfsr_time(current_delta as u64, true);

        // Check whether we have a stuck measurement (i.e. does the last
        // measurement holds entropy?).
        if ec.stuck(current_delta) { return None };

        // Rotate the data buffer by a prime number (any odd number would
        // do) to ensure that every bit position of the input time stamp
        // has an even chance of being merged with a bit position in the
        // entropy pool. We do not use one here as the adjacent bits in
        // successive time deltas may have some form of dependency. The
        // chosen value of 7 implies that the low 7 bits of the next
        // time delta value is concatenated with the current time delta.
        self.data = self.data.rotate_left(7);

        Some(())
    }

    // Shuffle the pool a bit by mixing some value with a bijective function
    // (XOR) into the pool.
    //
    // The function generates a mixer value that depends on the bits set and
    // the location of the set bits in the random number generated by the
    // entropy source. Therefore, based on the generated random number, this
    // mixer value can have 2^64 different values. That mixer value is
    // initialized with the first two SHA-1 constants. After obtaining the
    // mixer value, it is XORed into the random number.
    //
    // The mixer value is not assumed to contain any entropy. But due to the
    // XOR operation, it can also not destroy any entropy present in the
    // entropy pool.
    #[inline(never)]
    fn stir_pool(&mut self) {
        // This constant is derived from the first two 32 bit initialization
        // vectors of SHA-1 as defined in FIPS 180-4 section 5.3.1
        // The order does not really matter as we do not rely on the specific
        // numbers. We just pick the SHA-1 constants as they have a good mix of
        // bit set and unset.
        const CONSTANT: u64 = 0x67452301efcdab89;

        // The start value of the mixer variable is derived from the third
        // and fourth 32 bit initialization vector of SHA-1 as defined in
        // FIPS 180-4 section 5.3.1
        let mut mixer = 0x98badcfe10325476;

        // This is a constant time function to prevent leaking timing
        // information about the random number.
        // The normal code is:
        // ```
        // for i in 0..64 {
        //     if ((self.data >> i) & 1) == 1 { mixer ^= CONSTANT; }
        // }
        // ```
        // This is a bit fragile, as LLVM really wants to use branches here, and
        // we rely on it to not recognise the opportunity.
        for i in 0..64 {
            let apply = (self.data >> i) & 1;
            let mask = !apply.wrapping_sub(1);
            mixer ^= CONSTANT & mask;
            mixer = mixer.rotate_left(1);
        }

        self.data ^= mixer;
    }

    fn gen_entropy(&mut self) -> u64 {
        trace!("JitterRng: collecting entropy");

        // Prime `ec.prev_time`, and run the noice sources to make sure the
        // first loop round collects the expected entropy.
        let mut ec = EcState {
            prev_time: (self.timer)(),
            last_delta: 0,
            last_delta2: 0,
            mem: [0; MEMORY_SIZE],
        };
        let _ = self.measure_jitter(&mut ec);

        for _ in 0..self.rounds {
            // If a stuck measurement is received, repeat measurement
            // Note: we do not guard against an infinite loop, that would mean
            // the timer suddenly became broken.
            while self.measure_jitter(&mut ec).is_none() {}
        }

        // Do a single read from `self.mem` to make sure the Memory Access noise
        // source is not optimised out.
        black_box(ec.mem[0]);

        self.stir_pool();
        self.data
    }
    
    /// Basic quality tests on the timer, by measuring CPU timing jitter a few
    /// hundred times.
    ///
    /// If succesful, this will return the estimated number of rounds necessary
    /// to collect 64 bits of entropy. Otherwise a [`TimerError`] with the cause
    /// of the failure will be returned.
    ///
    /// [`TimerError`]: enum.TimerError.html
    #[cfg(not(all(target_arch = "wasm32", not(target_os = "emscripten"))))]
    pub fn test_timer(&mut self) -> Result<u8, TimerError> {
        debug!("JitterRng: testing timer ...");
        // We could add a check for system capabilities such as `clock_getres`
        // or check for `CONFIG_X86_TSC`, but it does not make much sense as the
        // following sanity checks verify that we have a high-resolution timer.

        let mut delta_sum = 0;
        let mut old_delta = 0;

        let mut time_backwards = 0;
        let mut count_mod = 0;
        let mut count_stuck = 0;

        let mut ec = EcState {
            prev_time: (self.timer)(),
            last_delta: 0,
            last_delta2: 0,
            mem: [0; MEMORY_SIZE],
        };

        // TESTLOOPCOUNT needs some loops to identify edge systems.
        // 100 is definitely too little.
        const TESTLOOPCOUNT: u64 = 300;
        const CLEARCACHE: u64 = 100;

        for i in 0..(CLEARCACHE + TESTLOOPCOUNT) {
            // Measure time delta of core entropy collection logic
            let time = (self.timer)();
            self.memaccess(&mut ec.mem, true);
            self.lfsr_time(time, true);
            let time2 = (self.timer)();

            // Test whether timer works
            if time == 0 || time2 == 0 {
                return Err(TimerError::NoTimer);
            }
            let delta = time2.wrapping_sub(time) as i64 as i32;

            // Test whether timer is fine grained enough to provide delta even
            // when called shortly after each other -- this implies that we also
            // have a high resolution timer
            if delta == 0 {
                return Err(TimerError::CoarseTimer);
            }

            // Up to here we did not modify any variable that will be
            // evaluated later, but we already performed some work. Thus we
            // already have had an impact on the caches, branch prediction,
            // etc. with the goal to clear it to get the worst case
            // measurements.
            if i < CLEARCACHE { continue; }

            if ec.stuck(delta) { count_stuck += 1; }

            // Test whether we have an increasing timer.
            if !(time2 > time) { time_backwards += 1; }

            // Count the number of times the counter increases in steps of 100ns
            // or greater.
            if (delta % 100) == 0 { count_mod += 1; }

            // Ensure that we have a varying delta timer which is necessary for
            // the calculation of entropy -- perform this check only after the
            // first loop is executed as we need to prime the old_delta value
            delta_sum += (delta - old_delta).abs() as u64;
            old_delta = delta;
        }

        // Do a single read from `self.mem` to make sure the Memory Access noise
        // source is not optimised out.
        black_box(ec.mem[0]);

        // We allow the time to run backwards for up to three times.
        // This can happen if the clock is being adjusted by NTP operations.
        // If such an operation just happens to interfere with our test, it
        // should not fail. The value of 3 should cover the NTP case being
        // performed during our test run.
        if time_backwards > 3 {
            return Err(TimerError::NotMonotonic);
        }

        // Test that the available amount of entropy per round does not get to
        // low. We expect 1 bit of entropy per round as a reasonable minimum
        // (although less is possible, it means the collector loop has to run
        // much more often).
        // `assert!(delta_average >= log2(1))`
        // `assert!(delta_sum / TESTLOOPCOUNT >= 1)`
        // `assert!(delta_sum >= TESTLOOPCOUNT)`
        if delta_sum < TESTLOOPCOUNT {
            return Err(TimerError::TinyVariantions);
        }

        // Ensure that we have variations in the time stamp below 100 for at
        // least 10% of all checks -- on some platforms, the counter increments
        // in multiples of 100, but not always
        if count_mod > (TESTLOOPCOUNT * 9 / 10) {
            return Err(TimerError::CoarseTimer);
        }

        // If we have more than 90% stuck results, then this Jitter RNG is
        // likely to not work well.
        if count_stuck > (TESTLOOPCOUNT * 9 / 10) {
            return Err(TimerError::TooManyStuck);
        }

        // Estimate the number of `measure_jitter` rounds necessary for 64 bits
        // of entropy.
        //
        // We don't try very hard to come up with a good estimate of the
        // available bits of entropy per round here for two reasons:
        // 1. Simple estimates of the available bits (like Shannon entropy) are
        //    too optimistic.
        // 2. Unless we want to waste a lot of time during intialization, there
        //    only a small number of samples are available.
        //
        // Therefore we use a very simple and conservative estimate:
        // `let bits_of_entropy = log2(delta_average) / 2`.
        //
        // The number of rounds `measure_jitter` should run to collect 64 bits
        // of entropy is `64 / bits_of_entropy`.
        let delta_average = delta_sum / TESTLOOPCOUNT;

        if delta_average >= 16 {
            let log2 = 64 - delta_average.leading_zeros();
            // Do something similar to roundup(64/(log2/2)):
            Ok( ((64u32 * 2 + log2 - 1) / log2) as u8)
        } else {
            // For values < 16 the rounding error becomes too large, use a
            // lookup table.
            // Values 0 and 1 are invalid, and filtered out by the
            // `delta_sum < TESTLOOPCOUNT` test above.
            let log2_lookup = [0,  0, 128, 81, 64, 56, 50, 46,
                               43, 41, 39, 38, 36, 35, 34, 33];
            Ok(log2_lookup[delta_average as usize])
        }
    }
    #[cfg(all(target_arch = "wasm32", not(target_os = "emscripten")))]
    pub fn test_timer(&mut self) -> Result<u8, TimerError> {
        return Err(TimerError::NoTimer);
    }

    /// Statistical test: return the timer delta of one normal run of the
    /// `JitterEntropy` entropy collector.
    ///
    /// Setting `var_rounds` to `true` will execute the memory access and the
    /// CPU jitter noice sources a variable amount of times (just like a real
    /// `JitterEntropy` round).
    ///
    /// Setting `var_rounds` to `false` will execute the noice sources the
    /// minimal number of times. This can be used to measure the minimum amount
    /// of entropy one round of entropy collector can collect in the worst case.
    ///
    /// # Example
    ///
    /// Use `timer_stats` to run the [NIST SP 800-90B Entropy Estimation Suite](
    /// https://github.com/usnistgov/SP800-90B_EntropyAssessment).
    ///
    /// This is the recommended way to test the quality of `JitterRng`. It
    /// should be run before using the RNG on untested hardware, after changes
    /// that could effect how the code is optimised, and after major compiler
    /// compiler changes, like a new LLVM version.
    ///
    /// First generate two files `jitter_rng_var.bin` and `jitter_rng_var.min`.
    ///
    /// Execute `python noniid_main.py -v jitter_rng_var.bin 8`, and validate it
    /// with `restart.py -v jitter_rng_var.bin 8 <min-entropy>`.
    /// This number is the expected amount of entropy that is at least available
    /// for each round of the entropy collector. This number should be greater
    /// than the amount estimated with `64 / test_timer()`.
    ///
    /// Execute `python noniid_main.py -v -u 4 jitter_rng_var.bin 4`, and
    /// validate it with `restart.py -v -u 4 jitter_rng_var.bin 4 <min-entropy>`.
    /// This number is the expected amount of entropy that is available in the
    /// last 4 bits of the timer delta after running noice sources. Note that
    /// a value of 3.70 is the minimum estimated entropy for true randomness.
    ///
    /// Execute `python noniid_main.py -v -u 4 jitter_rng_var.bin 4`, and
    /// validate it with `restart.py -v -u 4 jitter_rng_var.bin 4 <min-entropy>`.
    /// This number is the expected amount of entropy that is available to the
    /// entropy collecter if both noice sources only run their minimal number of
    /// times. This measures the absolute worst-case, and gives a lower bound
    /// for the available entropy.
    ///
    /// ```rust,no_run
    /// use rand::JitterRng;
    /// #
    /// # use std::error::Error;
    /// # use std::fs::File;
    /// # use std::io::Write;
    /// #
    /// # fn try_main() -> Result<(), Box<Error>> {
    /// let mut rng = JitterRng::new()?;
    ///
    /// // 1_000_000 results are required for the NIST SP 800-90B Entropy
    /// // Estimation Suite
    /// const ROUNDS: usize = 1_000_000;
    /// let mut deltas_variable: Vec<u8> = Vec::with_capacity(ROUNDS);
    /// let mut deltas_minimal: Vec<u8> = Vec::with_capacity(ROUNDS);
    ///
    /// for _ in 0..ROUNDS {
    ///     deltas_variable.push(rng.timer_stats(true) as u8);
    ///     deltas_minimal.push(rng.timer_stats(false) as u8);
    /// }
    ///
    /// // Write out after the statistics collection loop, to not disturb the
    /// // test results.
    /// File::create("jitter_rng_var.bin")?.write(&deltas_variable)?;
    /// File::create("jitter_rng_min.bin")?.write(&deltas_minimal)?;
    /// #
    /// # Ok(())
    /// # }
    /// #
    /// # fn main() {
    /// #     try_main().unwrap();
    /// # }
    /// ```
    ///
    #[cfg(feature="std")]
    pub fn timer_stats(&mut self, var_rounds: bool) -> i64 {
        let mut mem = [0; MEMORY_SIZE];

        let time = platform::get_nstime();
        self.memaccess(&mut mem, var_rounds);
        self.lfsr_time(time, var_rounds);
        let time2 = platform::get_nstime();
        time2.wrapping_sub(time) as i64
    }
}

#[cfg(feature="std")]
mod platform {
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows",
                  all(target_arch = "wasm32", not(target_os = "emscripten")))))]
    pub fn get_nstime() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};

        let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        // The correct way to calculate the current time is
        // `dur.as_secs() * 1_000_000_000 + dur.subsec_nanos() as u64`
        // But this is faster, and the difference in terms of entropy is
        // negligible (log2(10^9) == 29.9).
        dur.as_secs() << 30 | dur.subsec_nanos() as u64
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn get_nstime() -> u64 {
        extern crate libc;
        // On Mac OS and iOS std::time::SystemTime only has 1000ns resolution.
        // We use `mach_absolute_time` instead. This provides a CPU dependent
        // unit, to get real nanoseconds the result should by multiplied by
        // numer/denom from `mach_timebase_info`.
        // But we are not interested in the exact nanoseconds, just entropy. So
        // we use the raw result.
        unsafe { libc::mach_absolute_time() }
    }

    #[cfg(target_os = "windows")]
    pub fn get_nstime() -> u64 {
        extern crate winapi;
        unsafe {
            let mut t = super::mem::zeroed();
            winapi::um::profileapi::QueryPerformanceCounter(&mut t);
            *t.QuadPart() as u64
        }
    }

    #[cfg(all(target_arch = "wasm32", not(target_os = "emscripten")))]
    pub fn get_nstime() -> u64 {
        unreachable!()
    }
}

// A function that is opaque to the optimizer to assist in avoiding dead-code
// elimination. Taken from `bencher`.
fn black_box<T>(dummy: T) -> T {
    unsafe {
        let ret = ptr::read_volatile(&dummy);
        mem::forget(dummy);
        ret
    }
}

impl RngCore for JitterRng {
    fn next_u32(&mut self) -> u32 {
        // We want to use both parts of the generated entropy
        if self.data_half_used {
            self.data_half_used = false;
            (self.data >> 32) as u32
        } else {
            self.data = self.next_u64();
            self.data_half_used = true;
            self.data as u32
        }
    }

    fn next_u64(&mut self) -> u64 {
       self.data_half_used = false;
       self.gen_entropy()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // Fill using `next_u32`. This is faster for filling small slices (four
        // bytes or less), while the overhead is negligible.
        //
        // This is done especially for wrappers that implement `next_u32`
        // themselves via `fill_bytes`.
        impls::fill_bytes_via_u32(self, dest)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Error> {
        Ok(self.fill_bytes(dest))
    }
}

impl CryptoRng for JitterRng {}

#[cfg(test)]
mod test_jitter_init {
    use JitterRng;

    #[cfg(feature="std")]
    #[test]
    fn test_jitter_init() {
        use RngCore;
        // Because this is a debug build, measurements here are not representive
        // of the final release build.
        // Don't fail this test if initializing `JitterRng` fails because of a
        // bad timer (the timer from the standard library may not have enough
        // accuracy on all platforms).
        match JitterRng::new() {
            Ok(ref mut rng) => {
                // false positives are possible, but extremely unlikely
                assert!(rng.next_u32() | rng.next_u32() != 0);
            },
            Err(_) => {},
        }
    }

    #[test]
    fn test_jitter_bad_timer() {
        fn bad_timer() -> u64 { 0 }
        let mut rng = JitterRng::new_with_timer(bad_timer);
        assert!(rng.test_timer().is_err());
    }
}
