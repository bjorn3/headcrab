use std::{
    any::Any,
    sync::mpsc::{self, Receiver, SyncSender},
    sync::Arc,
    sync::Mutex,
};

#[derive(Clone)]
pub struct WorkerThread<T>(Arc<Mutex<WorkerThreadInner<T>>>);

pub struct WorkerThreadInner<T> {
    tx: SyncSender<Box<dyn FnOnce(&mut T) -> Box<dyn Any + Send> + Send>>,
    rx: Receiver<Box<dyn Any + Send>>,
}

impl<T: 'static> WorkerThread<T> {
    pub fn new<R: Send + 'static, E: Send + 'static>(
        f: impl FnOnce() -> Result<(T, R), E> + Send + 'static,
    ) -> Result<(Self, R), E> {
        let (tx_cmd, rx_cmd) =
            mpsc::sync_channel::<Box<dyn FnOnce(&mut T) -> Box<dyn Any + Send> + Send>>(0);
        let (tx_res, rx_res) = mpsc::sync_channel(0);

        std::thread::spawn(move || {
            let mut data = match f() {
                Ok((data, res)) => {
                    tx_res
                        .send(Box::new(Ok::<R, E>(res)) as Box<dyn Any + Send>)
                        .unwrap();
                    data
                }
                Err(err) => {
                    tx_res
                        .send(Box::new(Err::<R, E>(err)) as Box<dyn Any + Send>)
                        .unwrap();
                    return;
                }
            };

            loop {
                tx_res.send(rx_cmd.recv().unwrap()(&mut data)).unwrap();
            }
        });

        let status = (*rx_res.recv().unwrap().downcast::<Result<R, E>>().unwrap())?;

        Ok((
            WorkerThread(Arc::new(Mutex::new(WorkerThreadInner {
                tx: tx_cmd,
                rx: rx_res,
            }))),
            status,
        ))
    }

    pub fn spawn<R: Send + 'static>(&self, f: impl for<'a> FnOnce(&'a mut T) -> R + Send) -> R {
        let inner = self.0.lock().unwrap();
        inner
            .tx
            .send(unsafe {
                std::mem::transmute(Box::new(move |data: &mut T| {
                    Box::new(f(data)) as Box<dyn Any + Send>
                })
                    as Box<dyn for<'a> FnOnce(&'a mut T) -> Box<dyn Any + Send> + Send>)
            })
            .unwrap();
        *inner.rx.recv().unwrap().downcast::<R>().unwrap()
    }
}
