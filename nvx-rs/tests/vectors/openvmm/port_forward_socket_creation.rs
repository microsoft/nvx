    #[error("failed to create socket for port forward ({details})")]
    SocketCreation {
        #[source]
        source: std::io::Error,
        details: String,
    },
