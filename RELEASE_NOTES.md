# Release Notes

* 2019-??-??: 0.1.0-BETA
  * Added `async_send`, `receive_stream` and `peek_stream` functions which use Rust futures,
  now in beta. Until those features end up in stable `master` branch will require you build
  with the beta toolset. i.e. cargo +beta build.
* 2019-09-13: 0.0.10
  * Issue #13: A Deadlock would occur if the timeout occurred while waiting for space or data.
  * BREAKING CHANGE Timeouts are in `Duration` objects now rather than milliseconds.
* 2019-08-18: 0.0.9
  * Most `unsafe` code has been eliminated, enhancing stability.
  * Fix to Issue #10 involving a SEGFAULT.
* 2019-08-17: 0.0.8
  * Fixing issue #4
* 2019-08-17: 0.0.7
  * Removed `create_with_arcs()` function as `create()` is all that is needed now.
  * Made SeccSender and SeccReceiver both clonable as they have internal `Arc`s. 
* 2019-08-11: 0.0.6 
  * Improved README and Lib documentation.
* 2019-08-11: 0.0.1 
  * Migration from Axiom. SECC has been split off from Axiom into its own micro crate.
