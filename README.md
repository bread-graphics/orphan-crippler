# orphan-crippler

The `orphan-crippler` crate is designed to assist in building abstractions where work is offloaded to another
thread, for reasons such as blocking or OS-specific threads. For this reason, `orphan-crippler` implements the
Two-Way Oneshot (`two`) channel type, that allows one to send data to another thread and get more data in
response.

## Features

The optional `parking_lot` feature replaces the usual `std::sync::Mutex` usage in this crate with those from
the `parking_lot` crate. This pulls in a handful of other dependencies and is only really recommended if you are
already using `parking_lot` elsewhere in your application.

## License

MIT/Apache2 License
