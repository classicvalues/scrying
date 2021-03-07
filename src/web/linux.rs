/*
 *   This file is part of NCC Group Scrying https://github.com/nccgroup/scrying
 *   Copyright 2020 David Young <david(dot)young(at)nccgroup(dot)com>
 *   Released as open source by NCC Group Plc - https://www.nccgroup.com
 *
 *   Scrying is free software: you can redistribute it and/or modify
 *   it under the terms of the GNU General Public License as published by
 *   the Free Software Foundation, either version 3 of the License, or
 *   (at your option) any later version.
 *
 *   Scrying is distributed in the hope that it will be useful,
 *   but WITHOUT ANY WARRANTY; without even the implied warranty of
 *   MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 *   GNU General Public License for more details.
 *
 *   You should have received a copy of the GNU General Public License
 *   along with Scrying.  If not, see <https://www.gnu.org/licenses/>.
*/

use super::{save, HEIGHT, WIDTH};
use crate::{
    argparse::Opts, parsing::Target, reporting::ReportMessage, InputLists,
};
use gdk::prelude::WindowExtManual;
use gio::prelude::*;
use gtk::{
    Application, ApplicationWindow, ContainerExt, GtkWindowExt, WidgetExt,
    WindowPosition,
};
#[allow(unused)]
use log::{debug, error, info, trace, warn};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::{thread, time::Duration};
use webkit2gtk::{
    UserContentManager, WebContext, WebView, WebViewExt, WebViewExtManual,
};

enum GuiMessage {
    Navigate(String),
    Exit,
    PageReady,
}

pub fn web_worker(
    targets: Arc<InputLists>,
    opts: Arc<Opts>,
    report_tx: mpsc::Sender<ReportMessage>,
    caught_ctrl_c: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Create a window
    let application = Application::new(
        Some("com.github.nccgroup.scrying"),
        Default::default(),
    )?;

    // "global" bool to turn off the LoadEvent::Finished handler when
    // the target list has been exhausted
    let targets_exhausted = Arc::new(AtomicBool::new(false));
    let targets_exhausted_clone = targets_exhausted.clone();
    application.connect_activate(move |app| {
        let window = ApplicationWindow::new(app);
        window.set_default_size(WIDTH, HEIGHT);
        window.set_position(WindowPosition::Center);
        window.set_title("Scrying WebCapture");
        //window.set_visible(false); // this doesn't work for some reason

        // Create a webview
        let manager = UserContentManager::new();
        let context = WebContext::new();
        let webview = WebView::new_with_context_and_user_content_manager(
            &context, &manager,
        );

        // Make a channel for sending captured images back to the
        // supervisor thread
        let (img_tx, img_rx) = mpsc::channel::<Result<Vec<u8>, String>>();

        let targets_exhausted_clone = targets_exhausted_clone.clone();
        webview.connect_ready_to_show(move |_wv| {
            info!("Ready to show!");
            //img_tx.send(Ok(Vec::new())).unwrap();
        });

        // Create a communication channel
        let main_context = glib::MainContext::default();
        let (sender, receiver) =
            glib::MainContext::channel::<GuiMessage>(glib::Priority::default());

        let gui_sender = sender.clone();
        let (delayed_gui_sender, delayed_gui_receiver) =
            mpsc::channel::<GuiMessage>();

        thread::spawn(move || {
            while let Ok(msg) = delayed_gui_receiver.recv() {
                thread::sleep(Duration::from_millis(1000));
                gui_sender.send(msg).unwrap();
            }
        });

        webview.connect_load_changed(move |wv, evt| {
            use webkit2gtk::LoadEvent::*;
            trace!(
                "Webview event: {} from `{:?}`",
                evt,
                wv.get_uri().map(|s| s.as_str().to_string())
            );
            if targets_exhausted_clone.load(Ordering::SeqCst) {
                // no targets left to capture, so ignore this event
                trace!("Targets exhausted, ignoring event");
                return;
            }
            match evt {
                Finished => {
                    // grab screenshot
                    delayed_gui_sender.send(GuiMessage::PageReady).unwrap();
                }
                _ => {}
            }
        });

        window.add(&webview);
        window.show_all();

        receiver.attach(Some(&main_context), move |msg| match msg {
            GuiMessage::Navigate(u) => {
                trace!("Navigating to target: {}", u);
                webview.load_uri(&u);
                glib::source::Continue(true)
            }
            GuiMessage::Exit => {
                info!("Exit signal received, closing window");
                window.close();
                glib::source::Continue(false)
            }
            GuiMessage::PageReady => {
                if let Some(win) = webview.get_window() {
                    match win.get_pixbuf(0, 0, WIDTH, HEIGHT) {
                        Some(pix) => match pix.save_to_bufferv("png", &[]) {
                            Ok(buf) => {
                                trace!("Got pixbuf length {}", buf.len());
                                img_tx.send(Ok(buf)).unwrap();
                            }
                            Err(e) => {
                                img_tx
                                    .send(Err(format!(
                                        "Failed to process pixbuf: {}",
                                        e
                                    )))
                                    .unwrap();
                            }
                        },
                        None => {
                            img_tx
                                .send(Err(
                                    "Failed to retrieve pixbuf".to_string()
                                ))
                                .unwrap();
                        }
                    }
                } else {
                    img_tx
                        .send(Err("Unable to find window".to_string()))
                        .unwrap();
                }
                glib::source::Continue(true)
            }
        });

        let targets_clone = targets.clone();
        let report_tx_clone = report_tx.clone();
        let opts_clone = opts.clone();
        let targets_exhausted_clone = targets_exhausted.clone();
        let caught_ctrl_c_clone = caught_ctrl_c.clone();
        thread::spawn(move || {
            for target in &targets_clone.web_targets {
                // If ctrl+c has been pressed then don't send any more targets
                if caught_ctrl_c_clone.load(Ordering::SeqCst) {
                    break;
                }

                if let Target::Url(u) = target {
                    sender
                        .send(GuiMessage::Navigate(u.as_str().to_string()))
                        .unwrap();
                } else {
                    warn!("Target `{}` is not a URL!", target);
                    continue;
                }

                // Wait for a response
                match img_rx.recv() {
                    Ok(Ok(img)) => {
                        trace!("Screen capture received! (len {})", img.len());
                        save(
                            &target,
                            &opts_clone.output_dir,
                            &img,
                            &report_tx_clone,
                        )
                        .unwrap();
                    }
                    Ok(Err(e)) => {
                        warn!("Capture failed: {}", e);
                    }
                    Err(e) => {
                        warn!("Channel disconnected: {}", e);
                        break;
                    }
                }
            }

            // Reached end of input list - close the window
            trace!("Reached end of input list, sending window close request");
            targets_exhausted_clone.store(true, Ordering::SeqCst);
            sender.send(GuiMessage::Exit).unwrap();
            //end_of_targets_tx.send(()).unwrap();
        });
    });

    application.connect_shutdown(|_app| {
        debug!("application reached SHUTDOWN");
    });

    trace!("application.run");
    application.run(Default::default());
    trace!("End of web_worker function");
    Ok(())
}
