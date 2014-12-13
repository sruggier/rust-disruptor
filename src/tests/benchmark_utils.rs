extern crate getopts;

use std::task::TaskBuilder;
use native::task::NativeTaskBuilder;

/**
 * Spawn a task on a native thread.
 */
pub fn spawn_native(f: proc(): Send) {
    TaskBuilder::new().native().spawn(f);
}

/// Contains values obtained from common argument processing.
pub struct CommonTestOpts {
    pub n_iterations: u64,
}

fn usage(argv0: &str, opts: &[getopts::OptGroup]) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", getopts::usage(brief.as_slice(), opts));
    // Exit immediately
    panic!();
}

/**
 * Retrieve a parsed representation of the command-line arguments, or die trying. If the user has
 * requested a help string or given an invalid argument, this will print out help information and
 * exit.
 *
 * The API of this function will clearly have to change if some callers want to include their own
 * arguments, but for now, this will do.
 */
pub fn parse_args(default_n_iterations: u64) -> CommonTestOpts {
    use self::getopts::{optflag,optopt};

    let opts = vec!(
        optflag("h", "help", "show this message and exit"),
        optopt("n", "iterations",
            format!("how many iterations to perform in each benchmark (default {})",
            default_n_iterations).as_slice(), "N"),
    );

    let args = ::std::os::args();
    let arg_flags = args.tail();
    let argv0 = &args[0];

    let matches = match getopts::getopts(arg_flags, opts.as_slice()) {
        Ok(m) => m,
        Err(fail) => {
            println!("{}\nUse '{} --help' to see a list of valid options.", fail, argv0);
            panic!();
        }
    };
    if matches.opt_present("h") {
        usage(argv0.as_slice(), opts.as_slice());
    }

    // Validate as integer if -n specified
    let iterations = match matches.opt_str("n") {
        Some(n_str) => {
            match ::std::u64::parse_bytes(n_str.as_bytes(), 10u) {
                Some(n) => n,
                None => panic!("Expected a positive number of iterations, received {}", n_str)
            }
        }
        None => default_n_iterations
    };

    CommonTestOpts {
        n_iterations: iterations,
    }
}
