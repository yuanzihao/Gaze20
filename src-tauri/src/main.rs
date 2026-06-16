// Always use the Windows GUI subsystem so no console window ever appears —
// including debug builds. (Previously only release builds suppressed it, so a
// debug build popped a terminal whose closing also killed the app.)
#![windows_subsystem = "windows"]

fn main() {
    gaze20_lib::run()
}
