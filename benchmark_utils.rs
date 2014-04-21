extern crate getopts;

/// Contains values obtained from common argument processing.
pub struct CommonTestOpts {
    pub n_iterations: u64,
}

fn usage(argv0: &str, opts: ~[getopts::OptGroup]) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", getopts::usage(brief, opts));
    // Exit immediately
    fail!();
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

    let opts = ~[
        optflag("h", "help", "show this message and exit"),
        optopt("n", "iterations",
            format!("how many iterations to perform in each benchmark (default {})",
            default_n_iterations), "N"),
    ];

    let args = ::std::os::args();
    let arg_flags = args.tail();
    let argv0 = &args[0];

    let matches = match getopts::getopts(arg_flags, opts) {
        Ok(m) => m,
        Err(fail) => {
            println!("{}\nUse '{} --help' to see a list of valid options.", fail.to_err_msg(), *argv0);
            fail!();
        }
    };
    if matches.opt_present("h") {
        usage(*argv0, opts);
    }

    // Validate as integer if -n specified
    let iterations = match matches.opt_str("n") {
        Some(n_str) => {
            match ::std::u64::parse_bytes(n_str.as_bytes(), 10u) {
                Some(n) => n,
                None => fail!("Expected a positive number of iterations, received {}", n_str)
            }
        }
        None => default_n_iterations
    };

    CommonTestOpts {
        n_iterations: iterations,
    }
}
