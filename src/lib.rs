
use std::env;
use crossbeam_utils::thread;
use std::io;

/// Represents the types of errors that may occur while using cc-rs.
#[derive(Clone, Debug)]
enum ErrorKind {
    /// Error occurred while performing I/O.
    IOError,
}

/// Represents an internal error that occurred, with an explanation.
#[derive(Clone, Debug)]
pub struct Error {
    /// Describes the kind of error that occurred.
    kind: ErrorKind,
    /// More explanation of error that occurred.
    message: String,
}

impl Error {
    fn new(kind: ErrorKind, message: &str) -> Error {
        Error {
            kind: kind,
            message: message.to_owned(),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::new(ErrorKind::IOError, &format!("{}", e))
    }
}

fn compile_object<T, F: Fn(&T)>(f: F, obj: &T) -> Result<(), Error> {
    f(obj);
    Ok(())
}

pub fn compile_objects<T: 'static + Sync, F: Fn(&T) + Sync + Send>(f: &F, objs: &[T]) -> Result<(), Error> {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        use std::sync::Once;

        // Limit our parallelism globally with a jobserver. Start off by
        // releasing our own token for this process so we can have a bit of an
        // easier to write loop below. If this fails, though, then we're likely
        // on Windows with the main implicit token, so we just have a bit extra
        // parallelism for a bit and don't reacquire later.
        let server = jobserver();
        let reacquire = server.release_raw().is_ok();

        thread::scope(|s| {

        // When compiling objects in parallel we do a few dirty tricks to speed
        // things up:
        //
        // * First is that we use the `jobserver` crate to limit the parallelism
        //   of this build script. The `jobserver` crate will use a jobserver
        //   configured by Cargo for build scripts to ensure that parallelism is
        //   coordinated across C compilations and Rust compilations. Before we
        //   compile anything we make sure to wait until we acquire a token.
        //
        //   Note that this jobserver is cached globally so we only used one per
        //   process and only worry about creating it once.
        //
        // * Next we use a raw `thread::spawn` per thread to actually compile
        //   objects in parallel. We only actually spawn a thread after we've
        //   acquired a token to perform some work
        //
        // * Finally though we want to keep the dependencies of this crate
        //   pretty light, so we avoid using a safe abstraction like `rayon` and
        //   instead rely on some bits of `unsafe` code. We know that this stack
        //   frame persists while everything is compiling so we use all the
        //   stack-allocated objects without cloning/reallocating. We use a
        //   transmute to `State` with a `'static` lifetime to persist
        //   everything we need across the boundary, and the join-on-drop
        //   semantics of `JoinOnDrop` should ensure that our stack frame is
        //   alive while threads are alive.
        //
        // With all that in mind we compile all objects in a loop here, after we
        // acquire the appropriate tokens, Once all objects have been compiled
        // we join on all the threads and propagate the results of compilation.
        //
        // Note that as a slight optimization we try to break out as soon as
        // possible as soon as any compilation fails to ensure that errors get
        // out to the user as fast as possible.
        let error = AtomicBool::new(false);
        for obj in objs {
            if error.load(SeqCst) {
                break;
            }
            let token = server.acquire().unwrap();
            let state = State {
                obj,
                error: &error,
            };
            let state = unsafe { std::mem::transmute::<State<T>, State<'static, T>>(state) };
            s.spawn(|_| {
                let state: State<T> = state; // erase the `'static` lifetime
                let result = compile_object(f, state.obj);
                if result.is_err() {
                    state.error.store(true, SeqCst);
                }
                drop(token); // make sure our jobserver token is released after the compile
                return result;
            });
        }


    }).unwrap();

        // Reacquire our process's token before we proceed, which we released
        // before entering the loop above.
        if reacquire {
            server.acquire_raw()?;
        }

        return Ok(());

        /// Shared state from the parent thread to the child thread. This
        /// package of pointers is temporarily transmuted to a `'static`
        /// lifetime to cross the thread boundary and then once the thread is
        /// running we erase the `'static` to go back to an anonymous lifetime.
        struct State<'a, O> {
            obj: &'a O,
            error: &'a AtomicBool,
        }

        /// Returns a suitable `jobserver::Client` used to coordinate
        /// parallelism between build scripts.
        fn jobserver() -> &'static jobserver::Client {
            static INIT: Once = Once::new();
            static mut JOBSERVER: Option<jobserver::Client> = None;

            fn _assert_sync<T: Sync>() {}
            _assert_sync::<jobserver::Client>();

            unsafe {
                INIT.call_once(|| {
                    let server = default_jobserver();
                    JOBSERVER = Some(server);
                });
                JOBSERVER.as_ref().unwrap()
            }
        }

        unsafe fn default_jobserver() -> jobserver::Client {
            // Try to use the environmental jobserver which Cargo typically
            // initializes for us...
            if let Some(client) = jobserver::Client::from_env() {
                return client;
            }

            // ... but if that fails for whatever reason fall back to the number
            // of cpus on the system or the `NUM_JOBS` env var.
            let mut parallelism = num_cpus::get();
            if let Ok(amt) = env::var("NUM_JOBS") {
                if let Ok(amt) = amt.parse() {
                    parallelism = amt;
                }
            }

            // If we create our own jobserver then be sure to reserve one token
            // for ourselves.
            let client = jobserver::Client::new(parallelism).expect("failed to create jobserver");
            client.acquire_raw().expect("failed to acquire initial");
            return client;
        }

    }


#[test]
fn it_works() {
    struct Object;
    let mut v = Vec::new();
    for _ in 0..4000 {
        v.push(Object);
    }
    compile_objects(&|_| {
        println!("compile {:?}" ,    std::thread::current().id());

    }, &v).unwrap();
}
