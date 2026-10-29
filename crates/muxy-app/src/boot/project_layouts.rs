use muxy_app_core::project_layouts::{
    Config, DIRECTORY, Descriptor, LEGACY_DIRECTORY, discover, merge,
};
use muxy_client::Client;
use muxy_protocol::{FilesAction, FilesReply, FilesRequest, ProjectId, ServerPath};
use muxy_ui::tr;

pub(super) fn list(client: &Client, project: ProjectId) -> Result<Vec<Descriptor>, String> {
    let list = |directory: &str| match client
        .files(FilesRequest {
            project,
            action: FilesAction::List(ServerPath(directory.as_bytes().to_vec())),
        })
        .map_err(|error| error.to_string())?
    {
        FilesReply::Entries(entries) => Ok(discover(entries)),
        _ => Err(tr!("Unexpected layout directory response").to_string()),
    };
    match (list(DIRECTORY), list(LEGACY_DIRECTORY)) {
        (Ok(current), Ok(legacy)) => Ok(merge(current, legacy)),
        (Ok(layouts), Err(_)) | (Err(_), Ok(layouts)) => Ok(layouts),
        (Err(error), Err(_)) => Err(error),
    }
}

pub(super) fn load(
    client: &Client,
    project: ProjectId,
    layout: &Descriptor,
) -> Result<Config, String> {
    match client
        .files(FilesRequest {
            project,
            action: FilesAction::Read(layout.path.clone()),
        })
        .map_err(|error| error.to_string())?
    {
        FilesReply::Content(file) => Config::parse(&file.content),
        _ => Err(tr!("Unexpected layout file response").to_string()),
    }
}
