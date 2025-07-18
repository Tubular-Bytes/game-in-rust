pub enum Error {
    QueueEmptyError,
    ProcessError { message: String },
    InternalError { message: String },
}
