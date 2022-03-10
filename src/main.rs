mod csv;
mod find;
mod input;
mod ui;
#[allow(dead_code)]
mod util;
mod view;
use crate::input::{Control, InputHandler};
use crate::ui::{CsvTable, CsvTableState, FinderState};

extern crate csv as sushi_csv;

use anyhow::{Context, Result};
use clap::Parser;
use std::convert::TryInto;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::usize;
use tempfile::NamedTempFile;
use termion::{raw::IntoRawMode, screen::AlternateScreen};
use tui::backend::TermionBackend;
use tui::Terminal;

fn get_offsets_to_make_visible(
    found_record: find::FoundRecord,
    rows_view: &view::RowsView,
    csv_table_state: &CsvTableState,
) -> (Option<u64>, Option<u64>) {
    let new_rows_offset;
    // TODO: row_index() should probably be u64
    if rows_view.in_view(found_record.row_index() as u64) {
        new_rows_offset = None;
    } else {
        new_rows_offset = Some(found_record.row_index() as u64);
    }

    let new_cols_offset;
    let cols_offset = csv_table_state.cols_offset;
    let last_rendered_col = cols_offset.saturating_add(csv_table_state.num_cols_rendered);
    let column_index = found_record.first_column() as u64;
    if column_index >= cols_offset && column_index < last_rendered_col {
        new_cols_offset = None;
    } else {
        new_cols_offset = Some(column_index)
    }

    (new_rows_offset, new_cols_offset)
}

fn scroll_to_found_record(
    found_record: find::FoundRecord,
    rows_view: &mut view::RowsView,
    csv_table_state: &mut CsvTableState,
) {
    let (new_rows_offset, new_cols_offset) =
        get_offsets_to_make_visible(found_record.clone(), rows_view, csv_table_state);

    if let Some(rows_offset) = new_rows_offset {
        rows_view.set_rows_from(rows_offset).unwrap();
        csv_table_state.set_rows_offset(rows_offset);
    }

    if let Some(cols_offset) = new_cols_offset {
        csv_table_state.set_cols_offset(cols_offset);
    }
}

struct SeekableFile {
    filename: String,
    inner_file: Option<NamedTempFile>,
}

impl SeekableFile {
    fn new(filename: &str) -> Result<SeekableFile> {
        let mut f = File::open(filename).context(format!("Failed to open file: {}", filename))?;

        let mut inner_file = NamedTempFile::new()?;
        let inner_file_res;

        // If not seekable, it most likely is due to process substitution using
        // pipe - write out to a temp file to make it seekable
        if f.seek(SeekFrom::Start(0)).is_err() {
            let mut buffer: Vec<u8> = vec![];
            // TODO: could have read by chunks, yolo for now
            f.read_to_end(&mut buffer)?;
            inner_file.write(&buffer)?;
            inner_file_res = Some(inner_file);
        } else {
            inner_file_res = None;
        }

        Ok(SeekableFile {
            filename: filename.to_string(),
            inner_file: inner_file_res,
        })
    }

    fn filename(&self) -> &str {
        if let Some(f) = &self.inner_file {
            f.path().to_str().unwrap()
        } else {
            self.filename.as_str()
        }
    }
}

fn parse_delimiter(s: &str) -> Result<u8, &'static str> {
    let err = "Delimiter should be one ascii character";
    let mut iter = s.chars();
    match iter.next() {
        Some(c) if c.is_ascii() => {
            let c = c as u32;
            match iter.next() {
                Some(_) => Err(err),
                None => Ok(c.try_into().map_err(|_| err)?),
            }
        }
        _ => return Err(err),
    }
}

#[derive(Parser, Debug)]
struct Args {
    /// CSV filename
    filename: String,

    /// Show stats for debugging
    #[clap(long)]
    debug: bool,

    /// Delimiter to use for parsing the CSV file
    #[clap(long, short = 'd', parse(try_from_str = parse_delimiter))]
    delimiter: Option<u8>,
}

fn run_csvlens() -> Result<()> {
    let args = Args::parse();

    let show_stats = args.debug;

    let file = SeekableFile::new(args.filename.as_str())?;
    let filename = file.filename();

    // Some lines are reserved for plotting headers (3 lines for headers + 2 lines for status bar)
    let num_rows_not_visible = 5;

    // Number of rows that are visible in the current frame
    let num_rows = 50 - num_rows_not_visible;
    let csvlens_reader = csv::CsvLensReader::new(filename, args.delimiter)
        .context(format!("Failed to open file: {}", filename))?;
    let mut rows_view = view::RowsView::new(csvlens_reader, num_rows)?;

    let headers = rows_view.headers().clone();

    let stdout = io::stdout().into_raw_mode().unwrap();
    let stdout = AlternateScreen::from(stdout);
    let backend = TermionBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut input_handler = InputHandler::new();
    let mut csv_table_state = CsvTableState::new(filename.to_string(), headers.len());

    let mut finder: Option<find::Finder> = None;
    let mut first_found_scrolled = false;

    loop {
        terminal
            .draw(|f| {
                let size = f.size();

                // TODO: check type of num_rows too big?
                let frame_size_adjusted_num_rows =
                    size.height.saturating_sub(num_rows_not_visible as u16) as u64;
                rows_view
                    .set_num_rows(frame_size_adjusted_num_rows)
                    .unwrap();

                let rows = rows_view.rows();
                let csv_table = CsvTable::new(&headers, rows);

                f.render_stateful_widget(csv_table, size, &mut csv_table_state);
            })
            .unwrap();

        let control = input_handler.next();

        rows_view.handle_control(&control)?;

        match control {
            Control::Quit => {
                break;
            }
            Control::ScrollTo(_) => {
                csv_table_state.reset_buffer();
            }
            Control::ScrollLeft => {
                let new_cols_offset = csv_table_state.cols_offset.saturating_sub(1);
                csv_table_state.set_cols_offset(new_cols_offset);
            }
            Control::ScrollRight => {
                if csv_table_state.has_more_cols_to_show() {
                    let new_cols_offset = csv_table_state.cols_offset.saturating_add(1);
                    csv_table_state.set_cols_offset(new_cols_offset);
                }
            }
            Control::ScrollToNextFound if !rows_view.is_filter() => {
                if let Some(fdr) = finder.as_mut() {
                    if let Some(found_record) = fdr.next() {
                        scroll_to_found_record(found_record, &mut rows_view, &mut csv_table_state);
                    }
                }
            }
            Control::ScrollToPrevFound if !rows_view.is_filter() => {
                if let Some(fdr) = finder.as_mut() {
                    if let Some(found_record) = fdr.prev() {
                        scroll_to_found_record(found_record, &mut rows_view, &mut csv_table_state);
                    }
                }
            }
            Control::Find(s) => {
                finder = Some(find::Finder::new(filename, s.as_str()).unwrap());
                first_found_scrolled = false;
                rows_view.reset_filter().unwrap();
                csv_table_state.reset_buffer();
            }
            Control::Filter(s) => {
                finder = Some(find::Finder::new(filename, s.as_str()).unwrap());
                csv_table_state.reset_buffer();
                rows_view.set_rows_from(0).unwrap();
                rows_view.set_filter(finder.as_ref().unwrap()).unwrap();
            }
            Control::BufferContent(buf) => {
                csv_table_state.set_buffer(input_handler.mode(), buf.as_str());
            }
            Control::BufferReset => {
                csv_table_state.reset_buffer();
                if finder.is_some() {
                    finder = None;
                    csv_table_state.finder_state = FinderState::FinderInactive;
                    rows_view.reset_filter().unwrap();
                }
            }
            _ => {}
        }

        if let Some(fdr) = finder.as_mut() {
            if !rows_view.is_filter() {
                // scroll to first result once ready
                if !first_found_scrolled && fdr.count() > 0 {
                    // set row_hint to 0 so that this always scrolls to first result
                    fdr.set_row_hint(0);
                    if let Some(found_record) = fdr.next() {
                        scroll_to_found_record(found_record, &mut rows_view, &mut csv_table_state);
                    }
                    first_found_scrolled = true;
                }

                // reset cursor if out of view
                if let Some(cursor_row_index) = fdr.cursor_row_index() {
                    if !rows_view.in_view(cursor_row_index as u64) {
                        fdr.reset_cursor();
                    }
                }

                fdr.set_row_hint(rows_view.rows_from() as usize);
            } else {
                rows_view.set_filter(fdr).unwrap();
            }
        }

        // update rows and elapsed time if there are new results
        if let Some(elapsed) = rows_view.elapsed() {
            if show_stats {
                csv_table_state.elapsed = Some(elapsed as f64 / 1000.0);
            }
        }

        // TODO: is this update too late?
        csv_table_state.set_rows_offset(rows_view.rows_from());
        csv_table_state.selected = rows_view.selected();

        if let Some(n) = rows_view.get_total_line_numbers() {
            csv_table_state.set_total_line_number(n);
        } else if let Some(n) = rows_view.get_total_line_numbers_approx() {
            csv_table_state.set_total_line_number(n);
        }

        if let Some(f) = &finder {
            // TODO: need to create a new finder every time?
            csv_table_state.finder_state = FinderState::from_finder(f, &rows_view);
        }

        //csv_table_state.debug = format!("{:?}", rows_view.rows_from());
    }

    Ok(())
}

fn main() {
    if let Err(e) = run_csvlens() {
        println!("{}", e.to_string());
        std::process::exit(1);
    }
}
