// SPDX-License-Identifier: GPL-2.0-only

//! The boot banner: Bursztyn, the OchraOS mascot (a ginger Maine Coon with a pine collar).

use ochra_frame::color::{BOLD, OCHRE, PINE, RESET};
use ochra_frame::println;

/// Prints the cat and the system name.
pub fn print() {
    let version = env!("CARGO_PKG_VERSION");
    println!();
    println!(r"{OCHRE}     /\_____/\{RESET}");
    println!(r"{OCHRE}    /  o   o  \{RESET}       {PINE}{BOLD}OchraOS{RESET}");
    println!(r"{OCHRE}   ( ==  ^  == ){RESET}      kernel {version}");
    println!(r"{OCHRE}    ){PINE}  =====  {OCHRE}({RESET}");
    println!(r"{OCHRE}   (           ){RESET}");
    println!(r"{OCHRE}  ( (  )   (  ) ){RESET}");
    println!(r"{OCHRE} (__(__)___(__)__){RESET}");
    println!();
}
