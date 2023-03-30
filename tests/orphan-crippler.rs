// MIT/Apache2 License

#![allow(deprecated)]

use orphan_crippler::{two, Sender};
use std::{sync::mpsc, thread};

#[test]
fn mult_by_two_test() {
    const NUMBERS: &[i32] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const RESULTS: &[i32] = &[0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20];

    let (tasksend, taskrecv) = mpsc::channel::<Option<Sender<i32>>>();

    thread::spawn(move || loop {
        let task = taskrecv.recv().unwrap();
        match task {
            Some(mut task) => {
                let input = task.input().unwrap();
                task.send(input * 2);
            }
            None => break,
        }
    });

    let res = NUMBERS
        .iter()
        .copied()
        .map(|i| {
            let (sender, recv) = two::<i32, i32>(i);
            tasksend.send(Some(sender)).unwrap();
            recv.recv()
        })
        .collect::<Vec<i32>>();

    tasksend.send(None).unwrap();

    assert_eq!(res, RESULTS);
}
