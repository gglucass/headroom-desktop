class ContactMailer < ApplicationMailer
  def contact_confirmation(email)
    mail(
      to: email,
      subject: "Thank you for contacting Headroom"
    )
  end
end
