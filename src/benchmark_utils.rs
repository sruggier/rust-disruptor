use getopts::Options;

use std::str::FromStr;

/// Contains values obtained from common argument processing.
pub struct CommonTestOpts {
    pub n_iterations: u64,
}

fn usage(argv0: &str, opts: &Options) -> ! {
    let brief = format!("Usage: {} [OPTIONS]", argv0);
    println!("{}", opts.usage(&brief));
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
    let mut opts = Options::new();
    opts.optflag("h", "help", "show this message and exit");
    opts.optopt(
        "n",
        "iterations",
        &format!(
            "how many iterations to perform in each benchmark (default {})",
            default_n_iterations
        ),
        "N",
    );

    let args: Vec<String> = ::std::env::args().collect();
    let arg_flags = &args[1..];
    let argv0 = &args[0];

    let matches = match opts.parse(arg_flags) {
        Ok(m) => m,
        Err(fail) => {
            println!(
                "{}\nUse '{} --help' to see a list of valid options.",
                fail, argv0
            );
            panic!();
        }
    };
    if matches.opt_present("h") {
        usage(argv0, &opts);
    }

    // Validate as integer if -n specified
    let iterations = match matches.opt_str("n") {
        Some(n_str) => match u64::from_str(n_str.as_str()) {
            Ok(n) => n,
            Err(fail) => panic!("Failed to parse number of iterations '{}': {}", n_str, fail),
        },
        None => default_n_iterations,
    };

    CommonTestOpts {
        n_iterations: iterations,
    }
}
