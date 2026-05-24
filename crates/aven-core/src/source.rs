use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub usize);

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: FileId,
    pub path: Option<PathBuf>,
    pub name: String,
    pub source: String,
}

#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        &mut self,
        name: impl Into<String>,
        path: Option<PathBuf>,
        source: impl Into<String>,
    ) -> FileId {
        let id = FileId(self.files.len());
        self.files.push(SourceFile {
            id,
            path,
            name: name.into(),
            source: source.into(),
        });
        id
    }

    pub fn get(&self, id: FileId) -> Option<&SourceFile> {
        self.files.get(id.0)
    }
}
