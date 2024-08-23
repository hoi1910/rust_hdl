// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this file,
// You can obtain one at http://mozilla.org/MPL/2.0/.
//
// Copyright (c) 2018, Olof Kraigher olof.kraigher@gmail.com

mod completion;
mod diagnostics;
mod lifecycle;
mod rename;
mod text_document;
mod workspace;

use lsp_types::*;

use fnv::FnvHashMap;
use vhdl_lang::ast::ObjectClass;

use crate::rpc_channel::SharedRpcChannel;
use fuzzy_matcher::skim::SkimMatcherV2;
use std::io;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use vhdl_lang::{
    AnyEntKind, Concurrent, Config, EntHierarchy, EntRef, Message, MessageHandler, Object,
    Overloaded, Project, SeverityMap, SrcPos, Token, Type, VHDLStandard,
};

/// Defines how the language server handles files
/// that are not part of the `vhdl_ls.toml` project settings file.
#[derive(Default, Clone, Eq, PartialEq)]
pub enum NonProjectFileHandling {
    /// Ignore any non-project files
    Ignore,
    /// Add non-project files to an anonymous library and analyze them
    #[default]
    Analyze,
}

impl NonProjectFileHandling {
    pub fn from_string(value: &str) -> Option<NonProjectFileHandling> {
        use NonProjectFileHandling::*;
        Some(match value {
            "ignore" => Ignore,
            "analyze" => Analyze,
            _ => return None,
        })
    }
}

#[derive(Default, Clone)]
pub struct VHDLServerSettings {
    pub no_lint: bool,
    pub silent: bool,
    pub is_vscode: bool,
    pub non_project_file_handling: NonProjectFileHandling,
}

pub struct VHDLServer {
    rpc: SharedRpcChannel,
    settings: VHDLServerSettings,
    // To have well defined unit tests that are not affected by environment
    use_external_config: bool,
    project: Project,
    diagnostic_cache: FnvHashMap<Url, Vec<vhdl_lang::Diagnostic>>,
    init_params: Option<InitializeParams>,
    config_file: Option<PathBuf>,
    severity_map: SeverityMap,
    string_matcher: SkimMatcherV2,
}

impl VHDLServer {
    pub fn new_settings(rpc: SharedRpcChannel, settings: VHDLServerSettings) -> VHDLServer {
        VHDLServer {
            rpc,
            settings,
            use_external_config: true,
            project: Project::new(VHDLStandard::default()),
            diagnostic_cache: FnvHashMap::default(),
            init_params: None,
            config_file: None,
            severity_map: SeverityMap::default(),
            string_matcher: SkimMatcherV2::default().use_cache(true).ignore_case(),
        }
    }

    #[cfg(test)]
    fn new_external_config(rpc: SharedRpcChannel, use_external_config: bool) -> VHDLServer {
        VHDLServer {
            rpc,
            settings: Default::default(),
            use_external_config,
            project: Project::new(VHDLStandard::default()),
            diagnostic_cache: Default::default(),
            init_params: None,
            config_file: None,
            severity_map: SeverityMap::default(),
            string_matcher: SkimMatcherV2::default(),
        }
    }

    /// Load the workspace root configuration file
    fn load_root_uri_config(&self) -> io::Result<Config> {
        let config_file = self.config_file.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "Workspace root configuration file not set",
            )
        })?;
        let config = Config::read_file_path(config_file)?;

        // Log which file was loaded
        self.message(Message::log(format!(
            "Loaded workspace root configuration file: {}",
            config_file.to_str().unwrap()
        )));

        Ok(config)
    }

    /// Load the configuration or use a default configuration if unsuccessful
    /// Log info/error messages to the client
    fn load_config(&self) -> Config {
        let mut config = Config::default();

        if self.use_external_config {
            config.load_external_config(&mut self.message_filter(), None);
        }

        match self.load_root_uri_config() {
            Ok(root_config) => {
                config.append(&root_config, &mut self.message_filter());
            }
            Err(ref err) => {
                if matches!(err.kind(), ErrorKind::NotFound) {
                    self.message(Message::error(format!(
                        "Library mapping is unknown due to missing vhdl_ls.toml config file in the workspace root path: {err}"
                    )));
                    self.message(Message::warning(
                        "Without library mapping semantic analysis might be incorrect",
                    ));
                } else {
                    self.message(Message::error(format!("Error loading vhdl_ls.toml: {err}")));
                }
            }
        };

        config
    }

    /// Extract path of workspace root configuration file from InitializeParams
    fn root_uri_config_file(&self, params: &InitializeParams) -> Option<PathBuf> {
        #[allow(deprecated)]
        if is_vscode {
            match params.root_uri.clone() {
                Some(root_uri) => root_uri
                    .to_file_path()
                    .map(|root_path| root_path.join(".vscode").join("vhdl_ls.toml"))
                    .map_err(|_| {
                        self.message(Message::error(format!(
                            "{} {} {:?} ",
                            "Cannot load workspace:",
                            "initializeParams.rootUri is not a valid file path:",
                            root_uri,
                        )))
                    })
                    .ok(),
                None => {
                    self.message(Message::error(
                        "Cannot load workspace: Initialize request is missing rootUri parameter.",
                    ));
                    None
                }
            }
        } else {
            match params.root_uri.clone() {
                Some(root_uri) => root_uri
                    .to_file_path()
                    .map(|root_path| root_path.join("vhdl_ls.toml"))
                    .map_err(|_| {
                        self.message(Message::error(format!(
                            "{} {} {:?} ",
                            "Cannot load workspace:",
                            "initializeParams.rootUri is not a valid file path:",
                            root_uri,
                        )))
                    })
                    .ok(),
                None => {
                    self.message(Message::error(
                        "Cannot load workspace: Initialize request is missing rootUri parameter.",
                    ));
                    None
                }
            }
        }
    }

    fn client_supports_related_information(&self) -> bool {
        let try_fun = || {
            self.init_params
                .as_ref()?
                .capabilities
                .text_document
                .as_ref()?
                .publish_diagnostics
                .as_ref()?
                .related_information
        };
        try_fun().unwrap_or(false)
    }

    fn client_supports_did_change_watched_files(&self) -> bool {
        let try_fun = || {
            self.init_params
                .as_ref()?
                .capabilities
                .workspace
                .as_ref()?
                .did_change_watched_files
                .as_ref()?
                .dynamic_registration
        };
        try_fun().unwrap_or(false)
    }

    fn client_supports_snippets(&self) -> bool {
        let try_fun = || {
            self.init_params
                .as_ref()?
                .capabilities
                .text_document
                .as_ref()?
                .completion
                .as_ref()?
                .completion_item
                .as_ref()?
                .snippet_support
        };
        try_fun().unwrap_or(false)
    }

    fn client_has_hierarchical_document_symbol_support(&self) -> bool {
        let try_fun = || {
            self.init_params
                .as_ref()?
                .capabilities
                .text_document
                .as_ref()?
                .document_symbol
                .as_ref()?
                .hierarchical_document_symbol_support
        };
        try_fun().unwrap_or(false)
    }

    pub fn document_symbol(&self, params: &DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let source = self
            .project
            .get_source(&uri_to_file_name(&params.text_document.uri))?;

        // Some files are mapped to multiple libraries, only use the first library for document symbols
        let library_name = self
            .project
            .library_mapping_of(&source)
            .into_iter()
            .next()?;

        if self.client_has_hierarchical_document_symbol_support() {
            fn to_document_symbol(
                EntHierarchy { ent, children }: EntHierarchy,
                ctx: &Vec<Token>,
            ) -> DocumentSymbol {
                // Use the declaration position, if it exists,
                // else the position of the first source range token.
                // The latter is applicable for unnamed elements, e.g., processes or loops.
                let selection_pos = ent.decl_pos().unwrap_or(ent.src_span.start_token.pos(ctx));
                let src_range = ent.src_span.pos(ctx).range();
                #[allow(deprecated)]
                DocumentSymbol {
                    name: ent.describe(),
                    kind: to_symbol_kind(ent.kind()),
                    tags: None,
                    detail: None,
                    selection_range: to_lsp_range(selection_pos.range),
                    range: to_lsp_range(src_range),
                    children: if !children.is_empty() {
                        Some(
                            children
                                .into_iter()
                                .map(|hierarchy| to_document_symbol(hierarchy, ctx))
                                .collect(),
                        )
                    } else {
                        None
                    },
                    deprecated: None,
                }
            }

            Some(DocumentSymbolResponse::Nested(
                self.project
                    .document_symbols(&library_name, &source)
                    .into_iter()
                    .map(|(hierarchy, tokens)| to_document_symbol(hierarchy, tokens))
                    .collect(),
            ))
        } else {
            #[allow(clippy::ptr_arg)]
            fn to_symbol_information(ent: EntRef, ctx: &Vec<Token>) -> SymbolInformation {
                let selection_pos = ent.decl_pos().unwrap_or(ent.src_span.start_token.pos(ctx));
                #[allow(deprecated)]
                SymbolInformation {
                    name: ent.describe(),
                    kind: to_symbol_kind(ent.kind()),
                    tags: None,
                    location: srcpos_to_location(selection_pos),
                    deprecated: None,
                    container_name: ent.parent_in_same_source().map(|ent| ent.describe()),
                }
            }

            Some(DocumentSymbolResponse::Flat(
                self.project
                    .document_symbols(&library_name, &source)
                    .into_iter()
                    .flat_map(|(a, ctx)| {
                        a.into_flat()
                            .into_iter()
                            .map(|hierarchy| to_symbol_information(hierarchy, ctx))
                    })
                    .collect(),
            ))
        }
    }

    fn message_filter(&self) -> MessageFilter {
        MessageFilter {
            silent: self.settings.silent,
            rpc: self.rpc.clone(),
        }
    }

    fn message(&self, msg: Message) {
        self.message_filter().push(msg);
    }
}

struct MessageFilter {
    silent: bool,
    rpc: SharedRpcChannel,
}

impl MessageHandler for MessageFilter {
    fn push(&mut self, msg: Message) {
        if !self.silent
            && matches!(
                msg.message_type,
                vhdl_lang::MessageType::Warning | vhdl_lang::MessageType::Error
            )
        {
            self.rpc.send_notification(
                "window/showMessage",
                ShowMessageParams {
                    typ: to_lsp_message_type(&msg.message_type),
                    message: msg.message.clone(),
                },
            );
        }

        self.rpc.send_notification(
            "window/logMessage",
            LogMessageParams {
                typ: to_lsp_message_type(&msg.message_type),
                message: msg.message,
            },
        );
    }
}

fn to_lsp_message_type(message_type: &vhdl_lang::MessageType) -> MessageType {
    match message_type {
        vhdl_lang::MessageType::Error => MessageType::ERROR,
        vhdl_lang::MessageType::Warning => MessageType::WARNING,
        vhdl_lang::MessageType::Info => MessageType::INFO,
        vhdl_lang::MessageType::Log => MessageType::LOG,
    }
}

fn srcpos_to_location(pos: &SrcPos) -> Location {
    let uri = file_name_to_uri(pos.source.file_name());
    Location {
        uri,
        range: to_lsp_range(pos.range()),
    }
}

fn from_lsp_pos(position: lsp_types::Position) -> vhdl_lang::Position {
    vhdl_lang::Position {
        line: position.line,
        character: position.character,
    }
}

fn to_lsp_pos(position: vhdl_lang::Position) -> lsp_types::Position {
    lsp_types::Position {
        line: position.line,
        character: position.character,
    }
}

fn to_lsp_range(range: vhdl_lang::Range) -> lsp_types::Range {
    lsp_types::Range {
        start: to_lsp_pos(range.start),
        end: to_lsp_pos(range.end),
    }
}

fn from_lsp_range(range: lsp_types::Range) -> vhdl_lang::Range {
    vhdl_lang::Range {
        start: from_lsp_pos(range.start),
        end: from_lsp_pos(range.end),
    }
}

fn file_name_to_uri(file_name: &Path) -> Url {
    // @TODO return error to client
    Url::from_file_path(file_name).unwrap()
}

fn uri_to_file_name(uri: &Url) -> PathBuf {
    // @TODO return error to client
    uri.to_file_path().unwrap()
}

fn overloaded_kind(overloaded: &Overloaded) -> SymbolKind {
    match overloaded {
        Overloaded::SubprogramDecl(_) => SymbolKind::FUNCTION,
        Overloaded::Subprogram(_) => SymbolKind::FUNCTION,
        Overloaded::UninstSubprogramDecl(..) => SymbolKind::FUNCTION,
        Overloaded::UninstSubprogram(..) => SymbolKind::FUNCTION,
        Overloaded::InterfaceSubprogram(_) => SymbolKind::FUNCTION,
        Overloaded::EnumLiteral(_) => SymbolKind::ENUM_MEMBER,
        Overloaded::Alias(o) => overloaded_kind(o.kind()),
    }
}

fn object_kind(object: &Object) -> SymbolKind {
    if matches!(object.subtype.type_mark().kind(), Type::Protected(..)) {
        SymbolKind::OBJECT
    } else if object.iface.is_some() {
        SymbolKind::INTERFACE
    } else {
        object_class_kind(object.class)
    }
}

fn object_class_kind(class: ObjectClass) -> SymbolKind {
    match class {
        ObjectClass::Signal => SymbolKind::EVENT,
        ObjectClass::Constant => SymbolKind::CONSTANT,
        ObjectClass::Variable => SymbolKind::VARIABLE,
        ObjectClass::SharedVariable => SymbolKind::VARIABLE,
    }
}

fn type_kind(t: &Type) -> SymbolKind {
    match t {
        vhdl_lang::Type::Array { .. } => SymbolKind::ARRAY,
        vhdl_lang::Type::Enum(_) => SymbolKind::ENUM,
        vhdl_lang::Type::Integer => SymbolKind::NUMBER,
        vhdl_lang::Type::Real => SymbolKind::NUMBER,
        vhdl_lang::Type::Physical => SymbolKind::NUMBER,
        vhdl_lang::Type::Access(_) => SymbolKind::ENUM,
        vhdl_lang::Type::Record(_) => SymbolKind::STRUCT,
        vhdl_lang::Type::Incomplete => SymbolKind::NULL,
        vhdl_lang::Type::Subtype(t) => type_kind(t.type_mark().kind()),
        vhdl_lang::Type::Protected(_, _) => SymbolKind::CLASS,
        vhdl_lang::Type::File => SymbolKind::FILE,
        vhdl_lang::Type::Interface => SymbolKind::TYPE_PARAMETER,
        vhdl_lang::Type::Alias(t) => type_kind(t.kind()),
        vhdl_lang::Type::Universal(_) => SymbolKind::NUMBER,
    }
}

fn to_symbol_kind(kind: &AnyEntKind) -> SymbolKind {
    match kind {
        AnyEntKind::ExternalAlias { class, .. } => object_class_kind(ObjectClass::from(*class)),
        AnyEntKind::ObjectAlias { base_object, .. } => object_kind(base_object.object()),
        AnyEntKind::Object(o) => object_kind(o),
        AnyEntKind::LoopParameter(_) => SymbolKind::CONSTANT,
        AnyEntKind::PhysicalLiteral(_) => SymbolKind::CONSTANT,
        AnyEntKind::DeferredConstant(_) => SymbolKind::CONSTANT,
        AnyEntKind::File { .. } => SymbolKind::FILE,
        AnyEntKind::InterfaceFile { .. } => SymbolKind::INTERFACE,
        AnyEntKind::Component(_) => SymbolKind::CLASS,
        AnyEntKind::Attribute(_) => SymbolKind::PROPERTY,
        AnyEntKind::Overloaded(o) => overloaded_kind(o),
        AnyEntKind::Type(t) => type_kind(t),
        AnyEntKind::ElementDeclaration(_) => SymbolKind::FIELD,
        AnyEntKind::Sequential(_) => SymbolKind::NAMESPACE,
        AnyEntKind::Concurrent(Some(Concurrent::Instance)) => SymbolKind::MODULE,
        AnyEntKind::Concurrent(_) => SymbolKind::NAMESPACE,
        AnyEntKind::Library => SymbolKind::NAMESPACE,
        AnyEntKind::View(_) => SymbolKind::INTERFACE,
        AnyEntKind::Design(d) => match d {
            vhdl_lang::Design::Entity(_, _) => SymbolKind::MODULE,
            vhdl_lang::Design::Architecture(..) => SymbolKind::MODULE,
            vhdl_lang::Design::Configuration => SymbolKind::MODULE,
            vhdl_lang::Design::Package(_, _) => SymbolKind::PACKAGE,
            vhdl_lang::Design::PackageBody(..) => SymbolKind::PACKAGE,
            vhdl_lang::Design::UninstPackage(_, _) => SymbolKind::PACKAGE,
            vhdl_lang::Design::PackageInstance(_) => SymbolKind::PACKAGE,
            vhdl_lang::Design::InterfacePackageInstance(_) => SymbolKind::PACKAGE,
            vhdl_lang::Design::Context(_) => SymbolKind::NAMESPACE,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::rpc_channel::test_support::*;

    pub(crate) fn initialize_server(server: &mut VHDLServer, root_uri: Url) {
        let capabilities = ClientCapabilities::default();

        #[allow(deprecated)]
        let initialize_params = InitializeParams {
            process_id: None,
            root_path: None,
            root_uri: Some(root_uri),
            initialization_options: None,
            capabilities,
            trace: None,
            workspace_folders: None,
            client_info: None,
            locale: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };

        server.initialize_request(initialize_params);
        server.initialized_notification();
    }

    pub(crate) fn temp_root_uri() -> (tempfile::TempDir, Url) {
        let tempdir = tempfile::tempdir().unwrap();
        let root_uri = Url::from_file_path(tempdir.path().canonicalize().unwrap()).unwrap();
        (tempdir, root_uri)
    }

    pub(crate) fn expect_loaded_config_messages(mock: &RpcMock, config_uri: &Url) {
        let file_name = config_uri
            .to_file_path()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        mock.expect_message_contains(format!(
            "Loaded workspace root configuration file: {file_name}"
        ));
    }

    fn expect_missing_config_messages(mock: &RpcMock) {
        mock.expect_error_contains("Library mapping is unknown due to missing vhdl_ls.toml config file in the workspace root path");
        mock.expect_warning_contains(
            "Without library mapping semantic analysis might be incorrect",
        );
    }

    fn expect_erroneous_config(mock: &RpcMock) {
        mock.expect_error_contains("Error loading vhdl_ls.toml");
    }

    /// Create RpcMock and VHDLServer
    pub(crate) fn setup_server() -> (Rc<RpcMock>, VHDLServer) {
        let mock = Rc::new(RpcMock::new());
        let server = VHDLServer::new_external_config(SharedRpcChannel::new(mock.clone()), false);
        (mock, server)
    }

    #[test]
    fn initialize() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();
        expect_missing_config_messages(&mock);
        initialize_server(&mut server, root_uri);
    }

    #[test]
    fn did_open_no_diagnostics() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();
        expect_missing_config_messages(&mock);
        initialize_server(&mut server, root_uri.clone());

        let file_url = root_uri.join("ent.vhd").unwrap();
        let code = "
entity ent is
end entity ent;
"
        .to_owned();

        let did_open = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_url,
                language_id: "vhdl".to_owned(),
                version: 0,
                text: code,
            },
        };

        mock.expect_warning_contains("is not part of the project");

        server.text_document_did_open_notification(&did_open);
    }

    #[test]
    fn did_open_with_diagnostics_and_change_without() {
        let (mock, mut server) = setup_server();

        let (_tempdir, root_uri) = temp_root_uri();
        expect_missing_config_messages(&mock);
        initialize_server(&mut server, root_uri.clone());

        let file_url = root_uri.join("ent.vhd").unwrap();
        let code = "
entity ent is
end entity ent2;
"
        .to_owned();

        let did_open = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_url.clone(),
                language_id: "vhdl".to_owned(),
                version: 0,
                text: code,
            },
        };

        let publish_diagnostics = PublishDiagnosticsParams {
            uri: file_url.clone(),
            diagnostics: vec![lsp_types::Diagnostic {
                range: Range {
                    start: lsp_types::Position {
                        line: 2,
                        character: "end entity ".len() as u32,
                    },
                    end: lsp_types::Position {
                        line: 2,
                        character: "end entity ent2".len() as u32,
                    },
                },
                code: Some(NumberOrString::String("syntax_error".to_owned())),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("vhdl ls".to_owned()),
                message: "End identifier mismatch, expected ent".to_owned(),
                ..Default::default()
            }],
            version: None,
        };

        mock.expect_warning_contains("is not part of the project");

        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics);
        server.text_document_did_open_notification(&did_open);

        let code = "
entity ent is
end entity ent;
"
        .to_owned();

        let did_change = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: file_url.clone(),
                version: 1,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: code,
            }],
        };

        let publish_diagnostics = PublishDiagnosticsParams {
            uri: file_url,
            diagnostics: vec![],
            version: None,
        };

        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics);
        server.text_document_did_change_notification(&did_change);
    }

    pub(crate) fn write_file(
        root_uri: &Url,
        file_name: impl AsRef<str>,
        contents: impl AsRef<str>,
    ) -> Url {
        let path = root_uri.to_file_path().unwrap().join(file_name.as_ref());
        std::fs::write(&path, contents.as_ref()).unwrap();
        Url::from_file_path(path).unwrap()
    }

    pub(crate) fn write_config(root_uri: &Url, contents: impl AsRef<str>) -> Url {
        write_file(root_uri, "vhdl_ls.toml", contents)
    }

    #[test]
    fn initialize_with_config() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();
        let file_uri = write_file(
            &root_uri,
            "file.vhd",
            "\
entity ent is
end entity;

architecture rtl of ent2 is
begin
end;
",
        );

        let config_uri = write_config(
            &root_uri,
            "
[libraries]
lib.files = [
  'file.vhd'
]
",
        );

        let publish_diagnostics = PublishDiagnosticsParams {
            uri: file_uri,
            diagnostics: vec![lsp_types::Diagnostic {
                range: Range {
                    start: lsp_types::Position {
                        line: 3,
                        character: "architecture rtl of ".len() as u32,
                    },
                    end: lsp_types::Position {
                        line: 3,
                        character: "architecture rtl of ent2".len() as u32,
                    },
                },
                code: Some(NumberOrString::String("unresolved".to_owned())),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("vhdl ls".to_owned()),
                message: "No primary unit \'ent2\' within library \'lib\'".to_owned(),
                ..Default::default()
            }],
            version: None,
        };

        expect_loaded_config_messages(&mock, &config_uri);
        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics);

        initialize_server(&mut server, root_uri);
    }

    #[test]
    fn initialize_with_bad_config() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();

        write_config(
            &root_uri,
            "
[libraries
",
        );

        expect_erroneous_config(&mock);
        initialize_server(&mut server, root_uri);
    }

    #[test]
    fn initialize_with_config_missing_files() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();

        let config_uri = write_config(
            &root_uri,
            "
[libraries]
lib.files = [
'missing_file.vhd',
]
",
        );

        expect_loaded_config_messages(&mock, &config_uri);
        mock.expect_warning_contains("missing_file.vhd");
        initialize_server(&mut server, root_uri);
    }

    #[test]
    fn text_document_declaration() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();

        let file_url1 = write_file(
            &root_uri,
            "pkg1.vhd",
            "\
package pkg1 is
  type typ_t is (foo, bar);
end package;
",
        );

        let code2 = "\
use work.pkg1.all;
package pkg2 is
  constant c : typ_t := bar;
end package;
        "
        .to_owned();
        let file_url2 = write_file(&root_uri, "pkg2.vhd", &code2);

        let config_uri = write_config(
            &root_uri,
            format!(
                "
[libraries]
std.files = [
'{}/../vhdl_libraries/std/*.vhd',
]
lib.files = [
  '*.vhd'
]
",
                std::env::var("CARGO_MANIFEST_DIR").unwrap()
            ),
        );

        expect_loaded_config_messages(&mock, &config_uri);
        initialize_server(&mut server, root_uri);

        let did_open = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: file_url2.clone(),
                language_id: "vhdl".to_owned(),
                version: 0,
                text: code2,
            },
        };

        server.text_document_did_open_notification(&did_open);

        let response = server.text_document_declaration(&TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: file_url2 },
            position: lsp_types::Position {
                line: 2,
                character: "  constant c : t".len() as u32,
            },
        });

        let expected = Location {
            uri: file_url1,
            range: Range {
                start: lsp_types::Position {
                    line: 1,
                    character: "  type ".len() as u32,
                },
                end: lsp_types::Position {
                    line: 1,
                    character: "  type tpe_t".len() as u32,
                },
            },
        };

        assert_eq!(response, Some(expected));
    }

    #[test]
    fn client_register_capability() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();

        let config_uri = write_config(
            &root_uri,
            "
[libraries]
        ",
        );

        let register_options = DidChangeWatchedFilesRegistrationOptions {
            watchers: vec![FileSystemWatcher {
                glob_pattern: GlobPattern::String("**/vhdl_ls.toml".to_owned()),
                kind: None,
            }],
        };
        let register_capability = RegistrationParams {
            registrations: vec![Registration {
                id: "workspace/didChangeWatchedFiles".to_owned(),
                method: "workspace/didChangeWatchedFiles".to_owned(),
                register_options: serde_json::to_value(register_options).ok(),
            }],
        };

        expect_loaded_config_messages(&mock, &config_uri);
        mock.expect_request("client/registerCapability", register_capability);

        let capabilities = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                    dynamic_registration: Some(true),
                    relative_pattern_support: Some(false),
                }),
                ..WorkspaceClientCapabilities::default()
            }),
            ..ClientCapabilities::default()
        };
        #[allow(deprecated)]
        let initialize_params = InitializeParams {
            root_uri: Some(root_uri),
            capabilities,
            ..Default::default()
        };

        server.initialize_request(initialize_params);
        server.initialized_notification();
    }

    #[test]
    fn update_config_file() {
        let (mock, mut server) = setup_server();
        let (_tempdir, root_uri) = temp_root_uri();
        let file1_uri = write_file(
            &root_uri,
            "file1.vhd",
            "\
architecture rtl of ent is
begin
end;
",
        );
        let file2_uri = write_file(
            &root_uri,
            "file2.vhd",
            "\
architecture rtl of ent is
begin
end;
",
        );
        let config_uri = write_config(
            &root_uri,
            "
[libraries]
lib.files = [
  'file1.vhd'
]
",
        );

        let publish_diagnostics1 = PublishDiagnosticsParams {
            uri: file1_uri.clone(),
            diagnostics: vec![lsp_types::Diagnostic {
                range: Range {
                    start: lsp_types::Position {
                        line: 0,
                        character: "architecture rtl of ".len() as u32,
                    },
                    end: lsp_types::Position {
                        line: 0,
                        character: "architecture rtl of ent".len() as u32,
                    },
                },
                code: Some(NumberOrString::String("unresolved".to_owned())),
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("vhdl ls".to_owned()),
                message: "No primary unit \'ent\' within library \'lib\'".to_owned(),
                ..Default::default()
            }],
            version: None,
        };

        // after config change
        let publish_diagnostics2a = PublishDiagnosticsParams {
            uri: file2_uri,
            ..publish_diagnostics1.clone()
        };
        let publish_diagnostics2b = PublishDiagnosticsParams {
            uri: file1_uri,
            diagnostics: vec![],
            version: None,
        };

        expect_loaded_config_messages(&mock, &config_uri);
        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics1);
        mock.expect_message_contains("Configuration file has changed, reloading project...");
        expect_loaded_config_messages(&mock, &config_uri);
        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics2b);
        mock.expect_notification("textDocument/publishDiagnostics", publish_diagnostics2a);

        initialize_server(&mut server, root_uri.clone());

        let config_uri = write_config(
            &root_uri,
            "
[libraries]
lib.files = [
  'file2.vhd'
]
",
        );
        server.workspace_did_change_watched_files(&DidChangeWatchedFilesParams {
            changes: vec![FileEvent {
                typ: FileChangeType::CHANGED,
                uri: config_uri,
            }],
        });
    }
}
