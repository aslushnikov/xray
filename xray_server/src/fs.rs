use futures::{self, Future, Stream};
use ignore::WalkBuilder;
use parking_lot::Mutex;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use xray_core::cross_platform;
use xray_core::fs as xray_fs;
use xray_core::notify_cell::NotifyCell;

pub struct Tree {
    path: cross_platform::Path,
    root: xray_fs::Entry,
    updates: NotifyCell<()>,
    populated: NotifyCell<bool>,
}

pub struct FileProvider;

pub struct File {
    id: xray_fs::FileId,
    file: Arc<Mutex<fs::File>>,
}

impl Tree {
    pub fn new<T: Into<PathBuf>>(path: T) -> Result<Self, &'static str> {
        let path = path.into();
        let file_name = OsString::from(path.file_name().ok_or("Path must have a filename")?);
        let root = xray_fs::Entry::dir(file_name.into(), false, false);
        let updates = NotifyCell::new(());
        let populated = NotifyCell::new(false);
        Self::populate(
            path.clone(),
            root.clone(),
            updates.clone(),
            populated.clone(),
        );
        Ok(Self {
            path: cross_platform::Path::from(path.into_os_string()),
            root,
            updates,
            populated,
        })
    }

    fn populate(
        path: PathBuf,
        root: xray_fs::Entry,
        updates: NotifyCell<()>,
        populated: NotifyCell<bool>,
    ) {
        thread::spawn(move || {
            let mut stack = vec![root];

            let entries = WalkBuilder::new(path.clone())
                .follow_links(true)
                .include_ignored(true)
                .build()
                .skip(1)
                .filter_map(|e| e.ok());

            for entry in entries {
                stack.truncate(entry.depth());

                let file_type = entry.file_type().unwrap();
                let file_name = entry.file_name();

                if file_type.is_dir() {
                    let dir = xray_fs::Entry::dir(
                        file_name.into(),
                        file_type.is_symlink(),
                        entry.ignored(),
                    );
                    stack.last_mut().unwrap().insert(dir.clone()).unwrap();
                    stack.push(dir);
                } else if file_type.is_file() {
                    let file = xray_fs::Entry::file(
                        file_name.into(),
                        file_type.is_symlink(),
                        entry.ignored(),
                    );
                    stack.last_mut().unwrap().insert(file).unwrap();
                }
                updates.set(());
            }

            populated.set(true);
        });
    }
}

impl xray_fs::Tree for Tree {
    fn root(&self) -> xray_fs::Entry {
        self.root.clone()
    }

    fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
        Box::new(self.updates.observe())
    }
}

impl xray_fs::LocalTree for Tree {
    fn path(&self) -> &cross_platform::Path {
        &self.path
    }

    fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
        Box::new(
            self.populated
                .observe()
                .skip_while(|p| Ok(!p))
                .into_future()
                .then(|_| Ok(())),
        )
    }

    fn as_tree(&self) -> &xray_fs::Tree {
        self
    }
}

impl FileProvider {
    pub fn new() -> Self {
        FileProvider
    }
}

impl xray_fs::FileProvider for FileProvider {
    fn open(
        &self,
        path: &cross_platform::Path,
    ) -> Box<Future<Item = Box<xray_fs::File>, Error = io::Error>> {
        let path = path.to_path_buf();
        let (tx, rx) = futures::sync::oneshot::channel();

        thread::spawn(|| {
            fn open(path: PathBuf) -> Result<File, io::Error> {
                Ok(File::new(fs::File::open(path)?)?)
            }

            let _ = tx.send(open(path));
        });

        Box::new(
            rx.then(|result| result.expect("Sender should not be dropped"))
                .map(|file| Box::new(file) as Box<xray_fs::File>),
        )
    }
}

impl File {
    fn new(file: fs::File) -> Result<File, io::Error> {
        Ok(File {
            id: file.metadata()?.ino(),
            file: Arc::new(Mutex::new(file)),
        })
    }
}

impl xray_fs::File for File {
    fn id(&self) -> xray_fs::FileId {
        self.id
    }

    fn read(&self) -> Box<Future<Item = String, Error = io::Error>> {
        let (tx, rx) = futures::sync::oneshot::channel();
        let file = self.file.clone();
        thread::spawn(move || {
            fn read(file: &fs::File) -> Result<String, io::Error> {
                let mut buf_reader = io::BufReader::new(file);
                let mut contents = String::new();
                buf_reader.read_to_string(&mut contents)?;
                Ok(contents)
            }

            let _ = tx.send(read(&file.lock()));
        });

        Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
    }
}
