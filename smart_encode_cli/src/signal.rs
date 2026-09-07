// Phase 11: ctrlc handler + parking_lot::Mutex tracking the in-progress
// output path so SIGINT deletes the partial file before exit.
