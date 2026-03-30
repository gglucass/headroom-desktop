class ApplicationMailer < ActionMailer::Base
  default from: ENV.fetch("HEADROOM_CONTACT_FROM_EMAIL", "hello@example.com")
  layout "mailer"
end
