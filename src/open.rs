use std::path::PathBuf;

use toiletcli::flags;
use toiletcli::flags::*;

use crate::common::ResultS;
use crate::common::{
  BOLD,
  GREEN,
  PROGRAM_NAME,
  RESET,
};
use crate::common::{
  deserialize_docs_json,
  get_flag_error,
  get_terminal_width,
  is_docs_json_exists,
  is_docset_downloaded,
  make_sure_docset_is_in_docs,
  print_docset_file,
  print_page_from_docset,
  split_to_item_and_fragment,
};
use crate::translate::TranslationConfig;

fn show_open_help() -> ResultS
{
  println!(
           "\
{GREEN}USAGE{RESET}
    {BOLD}{PROGRAM_NAME} open{RESET} [-OPTIONS] <docset> <page>
    Print a page. Pages can be searched using `search`.

    {BOLD}{PROGRAM_NAME} open{RESET} [-OPTIONS] --html <HTML file>
    Translate an HTML file to text.

{GREEN}OPTIONS{RESET}
    -h, --html                      Interpret arguments as a path to HTML file
                                    and translate it to text.
    -c, --columns <number>          Make output N columns wide.
    -n, --line-numbers              Number outputted lines.
     -P, --only-show-path            Print path to the page and fragment on the
                                     second line instead of the page contents.
         --to <lang>                 Translate page to language (phase 1: only
                                     `vi` supported).
         --translator <name>         Translation provider (`google`, `lingva`).
                                     Default is `google`.
         --no-translation-cache      Bypass translation disk cache.
         --help                      Display help message."
  );
  Ok(())
}

pub(crate) fn open<Args>(mut args: Args) -> ResultS
  where Args: Iterator<Item = String>
{
  let mut flag_html;
  let mut flag_columns;
  let mut flag_number_lines;
  let mut flag_only_show_path;
  let mut flag_to;
  let mut flag_translator;
  let mut flag_no_translation_cache;
  let mut flag_help;

  let mut flags = flags![
    flag_html: BoolFlag,           ["-h", "--html"],
    flag_columns: StringFlag,      ["-c", "--columns"],
    flag_number_lines: BoolFlag,   ["-n", "--line-numbers"],
    flag_only_show_path: BoolFlag, ["-P", "--only-show-path"],
    flag_to: StringFlag,           ["--to"],
    flag_translator: StringFlag,   ["--translator"],
    flag_no_translation_cache: BoolFlag, ["--no-translation-cache"],
    flag_help: BoolFlag,           ["--help"]
  ];

  let args = parse_flags(&mut args, &mut flags).map_err(|err| get_flag_error(&err))?;
  if flag_help || args.is_empty() {
    return show_open_help();
  }

  let mut width = get_terminal_width();

  if let Ok(c) = flag_columns.parse::<usize>() {
    if c == 0 {
      width = 999;
    } else if c > 10 {
      width = c;
    } else {
      return Err("Invalid number of columns (less than 10).".to_string());
    }
  } else if !flag_columns.is_empty() {
    return Err("Invalid number of columns.".to_string());
  }

  let translation = if flag_to.is_empty() {
    None
  } else {
    if flag_to != "vi" {
      return Err("Only `--to vi` is supported in phase 1.".to_string());
    }
    let provider = if flag_translator.is_empty() { "google" } else { flag_translator.as_str() };
    if provider != "google" && provider != "lingva" {
      return Err("Unknown translator. Use `google` or `lingva`.".to_string());
    }
    Some(TranslationConfig::new(&flag_to, provider, flag_no_translation_cache))
  };

  if flag_html {
    let path = PathBuf::from(args.join(" "));
    if let Some(ref config) = translation {
      let pid = path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
      print_docset_file(path, None, width, flag_number_lines, Some(("html", &pid, config)))?;
    } else {
      print_docset_file(path, None, width, flag_number_lines, None)?;
    }
    return Ok(());
  }

  if !is_docs_json_exists()? {
    return Err(format!(
      "The list of available documents has not yet been downloaded. \
       Please run `{PROGRAM_NAME} fetch` first."
    ));
  }

  let mut args = args.into_iter();

  let docset = if let Some(docset_name) = args.next() {
    docset_name
  } else {
    return show_open_help();
  };

  if !is_docset_downloaded(&docset)? {
    make_sure_docset_is_in_docs(&docset, &deserialize_docs_json()?)?;
    return Err(format!("Docset `{docset}` is not downloaded. Try running \
                        `{PROGRAM_NAME} download {docset}`."));
  }

  let query = args.collect::<Vec<String>>().join(" ");
  if query.is_empty() {
    return Err("No page specified. Try `open --help` for more information.".to_string());
  }

  let (item, fragment) = split_to_item_and_fragment(query)?;
  print_page_from_docset(&docset,
                         &item,
                         fragment.as_ref(),
                         width,
                         flag_number_lines,
                         flag_only_show_path,
                         translation.as_ref())?;

  Ok(())
}
